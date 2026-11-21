//! The daily quality-upgrade check (`library.upgrade_quality`).
//!
//! Once a day per store account, for stores that declare
//! `CatalogCapabilities.qualityOffers`, the daemon walks the account's
//! acquired titles a page at a time, asks the store which stream the
//! account would download for each today, and compares it with the
//! original copy the library holds
//! ([`bookclerk_library::quality::is_upgrade`]). Each title that would get
//! a better file is queued as a forced, paced acquire, at most
//! `library.upgrade_daily_limit` per account per day; the download replaces
//! the held file and the library's other copies are made again from it.
//!
//! Each account's place in its holdings is kept between days
//! (`account_scan_state.quality_cursor`), so a large library is covered a
//! slice at a time without asking the store about every title every day.

use std::sync::Arc;
use std::time::Duration;

use bookclerk_library::control_plane::ScanRequester;
use bookclerk_library::quality::{held_stream, is_upgrade, StreamQuality};
use bookclerk_library::{AcquireStatus, AcquireTitle, JobTrigger, LibraryStore};
use bookclerk_source::ContentSource;
use chrono::{DateTime, Utc};
use tracing::{info, warn};

use crate::api::AppState;
use crate::job_worker::jittered_delay;
use crate::jobs::{enqueue_acquire, AcquireAsk};

/// How often the daemon looks for accounts whose daily check is due.
const QUALITY_UPGRADE_TICK: Duration = Duration::from_secs(60 * 60);

/// Shortest time between two checks of one account: a day, less an hour so
/// the hourly tick's jitter does not push a check to the day after.
pub(crate) const ACCOUNT_CHECK_INTERVAL: chrono::Duration = chrono::Duration::hours(23);

/// Most titles one account's daily check asks its store about.
pub(crate) const TITLES_PER_CHECK: usize = 512;

/// Holdings read per page, and so titles asked about per store call (at
/// most `maxListPage`).
const HOLDINGS_PAGE: u64 = 64;

/// Every hour, runs the quality-upgrade check for the accounts whose daily
/// check is due, while `library.upgrade_quality` is on.
pub fn spawn_quality_upgrade_loop(state: Arc<AppState>) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(jittered_delay(QUALITY_UPGRADE_TICK)).await;
            match queue_quality_upgrades(state.clone()).await {
                Ok(0) => {}
                Ok(queued) => info!(queued, "quality upgrades enqueued"),
                Err(err) => warn!(error = %err, "quality upgrade check failed"),
            }
        }
    });
}

/// Runs the check for every due account of every store that answers
/// quality offers, and queues the upgrades it finds.
///
/// # Returns
///
/// How many acquire jobs were admitted; 0 while `library.upgrade_quality`
/// is off.
///
/// # Errors
///
/// Returns an error when the accounts cannot be listed. One account's
/// failure is logged and the others still run.
pub async fn queue_quality_upgrades(state: Arc<AppState>) -> anyhow::Result<usize> {
    let (enabled, limit, download) = {
        let cfg = state.config.read().await;
        (
            cfg.library.upgrade_quality,
            cfg.library.upgrade_daily_limit,
            bookclerk_source::DownloadOptions::from(&*cfg),
        )
    };
    if !enabled {
        return Ok(0);
    }
    let sources: Vec<Arc<dyn ContentSource>> = state
        .sources
        .read()
        .await
        .routed()
        .into_iter()
        .filter(|source| source.answers_quality_offers())
        .collect();
    let library = state.library_snapshot().await;
    let mut admitted = 0;
    for source in sources {
        for account in library
            .list_accounts_for_plugin(source.plugin_key())
            .await?
        {
            if account.connection_status == "revoked" {
                continue;
            }
            let found = match find_upgrades(
                &library,
                source.as_ref(),
                &account.account_id,
                &download,
                limit,
                Utc::now(),
            )
            .await
            {
                Ok(found) => found,
                Err(err) => {
                    warn!(
                        plugin = %source.id(),
                        account_id = %account.account_id,
                        error = %err,
                        "quality upgrade check of an account failed"
                    );
                    continue;
                }
            };
            if found.is_empty() {
                continue;
            }
            let jobs = enqueue_acquire(
                state.clone(),
                ScanRequester::Operator,
                AcquireAsk {
                    titles: found,
                    account: Some(account.account_id.clone()),
                    plugin_instance_id: source.plugin_instance_id().map(str::to_owned),
                    force: true,
                    paced: true,
                    ..AcquireAsk::default()
                },
                JobTrigger::Scheduler,
            )
            .await?;
            admitted += jobs.len();
        }
    }
    Ok(admitted)
}

/// One account's daily check: the holding ids of the titles whose store
/// would now deliver a better file, at most `limit` of them.
///
/// Asks about at most [`TITLES_PER_CHECK`] acquired titles, continuing
/// after where the last check stopped and starting over after the
/// account's last holding. Only titles whose files this account's holding
/// fetched are compared (a sibling account's or an upload's files are not
/// this store's to replace). The place reached and the time are saved, so
/// the account is not checked again for [`ACCOUNT_CHECK_INTERVAL`]; a
/// store call that fails saves the place before it and fails the check.
///
/// # Arguments
///
/// * `library` - The library store.
/// * `source` - The account's store; it must answer quality offers.
/// * `account_id` - The account.
/// * `download` - The download options the instance would fetch with, so
///   the store answers for the stream an acquire would actually get.
/// * `limit` - Most upgrades to find.
/// * `now` - The check's time.
///
/// # Returns
///
/// The holding ids to download again, in holding order; empty when the
/// check is not due, the account was never scanned, or nothing is better.
///
/// # Errors
///
/// Returns the read, store-call or write error.
pub(crate) async fn find_upgrades(
    library: &LibraryStore,
    source: &dyn ContentSource,
    account_id: &str,
    download: &bookclerk_source::DownloadOptions,
    limit: u32,
    now: DateTime<Utc>,
) -> anyhow::Result<Vec<String>> {
    let plugin_key = source.plugin_key();
    let Some(state) = library.quality_check_state(plugin_key, account_id).await? else {
        return Ok(Vec::new());
    };
    if state
        .checked_at
        .is_some_and(|at| now - at < ACCOUNT_CHECK_INTERVAL)
    {
        return Ok(Vec::new());
    }
    let limit = usize::try_from(limit).unwrap_or(usize::MAX);
    let scope = bookclerk_source::source_scope(library, source);
    let mut cursor = (!state.cursor.is_empty()).then_some(state.cursor);
    let mut asked = 0;
    let mut upgrades = Vec::new();
    while asked < TITLES_PER_CHECK && upgrades.len() < limit {
        let page = library
            .acquire_titles_page(Some(account_id), cursor.as_deref(), HOLDINGS_PAGE)
            .await?;
        let mut held: Vec<(AcquireTitle, StreamQuality)> = Vec::new();
        for title in page.titles {
            if title.plugin_key != plugin_key
                || title.account_id != account_id
                || title.status != AcquireStatus::Acquired
                || !title.acquired_here()
            {
                continue;
            }
            let files = library.edition_audio_files(&title.edition_id).await?;
            if let Some(stream) = held_stream(&files) {
                held.push((title, stream));
            }
        }
        if !held.is_empty() {
            let ids: Vec<String> = held.iter().map(|(t, _)| t.product_id.clone()).collect();
            let offers = match source
                .quality_offers(&scope, account_id, &ids, download)
                .await
            {
                Ok(offers) => offers,
                Err(err) => {
                    library
                        .save_quality_check(
                            plugin_key,
                            account_id,
                            cursor.as_deref().unwrap_or(""),
                            now,
                        )
                        .await?;
                    return Err(err.into());
                }
            };
            asked += held.len();
            for ((title, stream), offer) in held.iter().zip(offers) {
                if upgrades.len() >= limit {
                    break;
                }
                if offer.is_some_and(|offer| is_upgrade(stream, &offer)) {
                    upgrades.push(title.holding_id.clone());
                }
            }
            if upgrades.len() >= limit {
                // Continue after the last upgrade next time, so the titles
                // after it on this page are asked about again.
                cursor = upgrades.last().cloned();
                break;
            }
        }
        cursor = page.next;
        if cursor.is_none() {
            break;
        }
    }
    library
        .save_quality_check(plugin_key, account_id, cursor.as_deref().unwrap_or(""), now)
        .await?;
    Ok(upgrades)
}

#[cfg(test)]
#[path = "quality_upgrades_tests.rs"]
mod tests;
