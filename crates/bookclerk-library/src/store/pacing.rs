//! Download pacing state per store account: the override, the store's
//! cool-down, and the ledger of downloads ([`crate::pacing`] holds the
//! rules).
//!
//! Every write is one atomic host batch through the canonical SQL-v1
//! contract: `UPDATE` then `INSERT OR IGNORE`, because SQL-v1 has no
//! `ON CONFLICT … DO UPDATE`. Rows are written only while the account row
//! exists (its foreign key), so a write for an account that was never
//! recorded is a no-op rather than an error.

use chrono::{DateTime, Utc};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect};

use super::LibraryStore;
use crate::control_plane::batch;
use crate::entities::{account_downloads, account_pace};
use crate::error::{LibraryError, Result};
use crate::pacing::{DownloadWindow, PaceOverride, BUDGET_WINDOW, LEDGER_RETENTION};

/// Who started a download, for the ledger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadStarter {
    /// The host started it (auto-acquire, upgrades, acquire-all, requests
    /// from other libraries): held for the pace.
    Host,
    /// A person started it by hand: counted, never held.
    Person,
}

/// One store download's ledger identity.
///
/// It is made once, when a download is kept for recording, and every write
/// attempt of that download passes the same one, so a write that committed
/// but whose answer was lost inserts no second row when it is tried again.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DownloadId(String);

impl DownloadId {
    /// A fresh identity (a random UUID) for a new download.
    #[must_use]
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }

    /// The identity as the ledger's `id` column stores it.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for DownloadId {
    fn default() -> Self {
        Self::new()
    }
}

/// One account's pacing state as stored.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AccountPaceState {
    /// The operator's override of the storefront's pace.
    pub override_: PaceOverride,
    /// End of the store's cool-down, when one is set (it may be past).
    pub cooldown_until: Option<DateTime<Utc>>,
    /// Downloads in the last [`BUDGET_WINDOW`], oldest first.
    pub window: DownloadWindow,
}

/// `chrono` milliseconds for a std duration, saturating.
fn millis(d: std::time::Duration) -> i64 {
    i64::try_from(d.as_millis()).unwrap_or(i64::MAX)
}

/// A UTC time from Unix milliseconds; `None` when out of range.
fn from_ms(ms: i64) -> Option<DateTime<Utc>> {
    DateTime::from_timestamp_millis(ms)
}

/// Most ledger rows read in one page, below the database's own result cap.
const LEDGER_PAGE_ROWS: u64 = 256;

/// `WHERE` guard that keeps a write to rows of an account that exists.
const ACCOUNT_EXISTS: &str =
    " AND EXISTS (SELECT 1 FROM accounts WHERE plugin_key = ? AND account_id = ?)";

impl LibraryStore {
    /// Reads one account's pacing state at `now`.
    ///
    /// # Arguments
    ///
    /// * `plugin_key` - The account's plugin.
    /// * `account_id` - The account.
    /// * `now` - The current time (the window ends here).
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when a read fails.
    pub async fn account_pace_state(
        &self,
        plugin_key: &str,
        account_id: &str,
        now: DateTime<Utc>,
    ) -> Result<AccountPaceState> {
        let row = account_pace::Entity::find()
            .filter(account_pace::Column::PluginKey.eq(plugin_key))
            .filter(account_pace::Column::AccountId.eq(account_id))
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?;
        let since = now.timestamp_millis().saturating_sub(millis(BUDGET_WINDOW));
        let starts = self.window_starts(plugin_key, account_id, since).await?;
        let (override_, cooldown_until) = row.map_or((PaceOverride::Default, None), |r| {
            (
                PaceOverride::from_row(&r.pace_mode, r.downloads_per_day, r.min_gap_seconds),
                r.cooldown_until_ms.and_then(from_ms),
            )
        });
        Ok(AccountPaceState {
            override_,
            cooldown_until,
            window: DownloadWindow { starts },
        })
    }

    /// The start times of an account's downloads after `since_ms`, oldest
    /// first, read in pages no larger than the database's result cap.
    ///
    /// Pages are keyed on `(started_at_ms, id)`, so downloads that started
    /// in the same millisecond are neither skipped nor read twice.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Orm`] when a read fails.
    async fn window_starts(
        &self,
        plugin_key: &str,
        account_id: &str,
        since_ms: i64,
    ) -> Result<Vec<DateTime<Utc>>> {
        let page = match self.db_capabilities().max_result_rows {
            0 => LEDGER_PAGE_ROWS,
            rows => LEDGER_PAGE_ROWS.min(u64::from(rows)).max(1),
        };
        let mut starts = Vec::new();
        let mut after: Option<(i64, String)> = None;
        loop {
            let mut query = account_downloads::Entity::find()
                .filter(account_downloads::Column::PluginKey.eq(plugin_key))
                .filter(account_downloads::Column::AccountId.eq(account_id))
                .filter(account_downloads::Column::StartedAtMs.gt(since_ms))
                .order_by_asc(account_downloads::Column::StartedAtMs)
                .order_by_asc(account_downloads::Column::Id)
                .limit(page);
            if let Some((ms, id)) = &after {
                query = query.filter(
                    sea_orm::Condition::any()
                        .add(account_downloads::Column::StartedAtMs.gt(*ms))
                        .add(
                            sea_orm::Condition::all()
                                .add(account_downloads::Column::StartedAtMs.eq(*ms))
                                .add(account_downloads::Column::Id.gt(id.as_str())),
                        ),
                );
            }
            let rows = query.all(&self.db).await.map_err(LibraryError::Orm)?;
            let full = u64::try_from(rows.len()).unwrap_or(u64::MAX) >= page;
            after = rows.last().map(|r| (r.started_at_ms, r.id.clone()));
            starts.extend(rows.into_iter().filter_map(|r| from_ms(r.started_at_ms)));
            if !full || after.is_none() {
                return Ok(starts);
            }
        }
    }

    /// Stores the operator's override of an account's pace (the cool-down
    /// is kept), and wakes parked paced acquires that may download on it so
    /// they look at the new pace ([`Self::wake_paced_acquires`]).
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Other`] when the override is out of range
    /// ([`PaceOverride::validate`]), or an error when the batch fails.
    pub async fn set_account_pace_override(
        &self,
        plugin_key: &str,
        account_id: &str,
        override_: &PaceOverride,
    ) -> Result<()> {
        override_
            .validate()
            .map_err(|err| LibraryError::Other(anyhow::anyhow!("{err}")))?;
        let (per_day, gap) = match override_ {
            PaceOverride::Custom {
                downloads_per_day,
                min_gap_seconds,
            } => (
                downloads_per_day.map(|n| i64::from(n.get())),
                min_gap_seconds.map(i64::from),
            ),
            PaceOverride::Default | PaceOverride::Unlimited => (None, None),
        };
        let now = super::now_str();
        let mode = override_.mode_str();
        self.write_account_pace(
            "pace",
            "UPDATE account_pace SET pace_mode = ?, downloads_per_day = ?, \
             min_gap_seconds = ?, updated_at = ? WHERE plugin_key = ? AND account_id = ?",
            vec![
                batch::text(mode),
                batch::int_opt(per_day),
                batch::int_opt(gap),
                batch::text(&now),
                batch::text(plugin_key),
                batch::text(account_id),
            ],
            plugin_key,
            account_id,
            mode,
            per_day,
            gap,
            None,
            &now,
        )
        .await?;
        self.wake_paced_acquires(account_id).await?;
        Ok(())
    }

    /// Records the store's cool-down for an account, unless one that ends
    /// later is already recorded.
    ///
    /// # Errors
    ///
    /// Returns an error when the batch fails.
    pub async fn extend_account_cooldown(
        &self,
        plugin_key: &str,
        account_id: &str,
        until: DateTime<Utc>,
    ) -> Result<()> {
        let now = super::now_str();
        let until_ms = until.timestamp_millis();
        self.write_account_pace(
            "cooldown",
            "UPDATE account_pace SET cooldown_until_ms = ?, updated_at = ? \
             WHERE plugin_key = ? AND account_id = ? \
             AND (cooldown_until_ms IS NULL OR cooldown_until_ms < ?)",
            vec![
                batch::int(until_ms),
                batch::text(&now),
                batch::text(plugin_key),
                batch::text(account_id),
                batch::int(until_ms),
            ],
            plugin_key,
            account_id,
            PaceOverride::Default.mode_str(),
            None,
            None,
            Some(until_ms),
            &now,
        )
        .await
    }

    /// Clears an account's cool-down (the operator knows better), and wakes
    /// parked paced acquires that may download on it
    /// ([`Self::wake_paced_acquires`]).
    ///
    /// # Errors
    ///
    /// Returns an error when the batch fails.
    pub async fn clear_account_cooldown(&self, plugin_key: &str, account_id: &str) -> Result<()> {
        let now = super::now_str();
        let operation_id = format!("account-pace-clear-{}", uuid::Uuid::new_v4());
        self.execute_host_batch(batch::request(
            &operation_id,
            vec![batch::exec(
                "UPDATE account_pace SET cooldown_until_ms = NULL, updated_at = ? \
                 WHERE plugin_key = ? AND account_id = ?",
                vec![
                    batch::text(&now),
                    batch::text(plugin_key),
                    batch::text(account_id),
                ],
            )],
        ))
        .await?;
        self.wake_paced_acquires(account_id).await?;
        Ok(())
    }

    /// Records a download that reached the store, and prunes the account's
    /// ledger rows older than [`LEDGER_RETENTION`], in one batch.
    ///
    /// Writing the same `id` again adds no second row, so a write whose
    /// answer was lost may be retried.
    ///
    /// # Arguments
    ///
    /// * `id` - The download's identity, the same for every write attempt.
    /// * `plugin_key` - The account's plugin.
    /// * `account_id` - The account.
    /// * `started_at` - When the download started.
    /// * `job_id` - The acquire job that ran it, if any.
    /// * `starter` - Whether the host or a person started it.
    ///
    /// # Errors
    ///
    /// Returns an error when the batch fails.
    pub async fn record_account_download(
        &self,
        id: &DownloadId,
        plugin_key: &str,
        account_id: &str,
        started_at: DateTime<Utc>,
        job_id: Option<&str>,
        starter: DownloadStarter,
    ) -> Result<()> {
        let started_ms = started_at.timestamp_millis();
        let prune_before = started_ms.saturating_sub(millis(LEDGER_RETENTION));
        let paced = match starter {
            DownloadStarter::Host => 1,
            DownloadStarter::Person => 0,
        };
        let operation_id = format!("account-download-{}", id.as_str());
        self.execute_host_batch(batch::request(
            &operation_id,
            vec![
                batch::exec(
                    "DELETE FROM account_downloads WHERE plugin_key = ? AND account_id = ? \
                     AND started_at_ms < ?",
                    vec![
                        batch::text(plugin_key),
                        batch::text(account_id),
                        batch::int(prune_before),
                    ],
                ),
                batch::exec(
                    &format!(
                        "INSERT OR IGNORE INTO account_downloads \
                         (id, plugin_key, account_id, started_at_ms, job_id, paced) \
                         SELECT ?, ?, ?, ?, ?, ? WHERE 1 = 1{ACCOUNT_EXISTS}"
                    ),
                    vec![
                        batch::text(id.as_str()),
                        batch::text(plugin_key),
                        batch::text(account_id),
                        batch::int(started_ms),
                        batch::text_opt(job_id),
                        batch::int(paced),
                        batch::text(plugin_key),
                        batch::text(account_id),
                    ],
                ),
            ],
        ))
        .await?;
        Ok(())
    }

    /// Runs `update` and the insert of a missing `account_pace` row as one
    /// batch. The insert carries the values a fresh row should have.
    #[allow(clippy::too_many_arguments)]
    async fn write_account_pace(
        &self,
        what: &str,
        update: &str,
        update_params: Vec<bookclerk_plugin_abi::DbValue>,
        plugin_key: &str,
        account_id: &str,
        mode: &str,
        per_day: Option<i64>,
        gap: Option<i64>,
        cooldown_until_ms: Option<i64>,
        now: &str,
    ) -> Result<()> {
        let insert = batch::exec(
            &format!(
                "INSERT OR IGNORE INTO account_pace \
                 (plugin_key, account_id, pace_mode, downloads_per_day, min_gap_seconds, \
                  cooldown_until_ms, updated_at) \
                 SELECT ?, ?, ?, ?, ?, ?, ? WHERE 1 = 1{ACCOUNT_EXISTS}"
            ),
            vec![
                batch::text(plugin_key),
                batch::text(account_id),
                batch::text(mode),
                batch::int_opt(per_day),
                batch::int_opt(gap),
                batch::int_opt(cooldown_until_ms),
                batch::text(now),
                batch::text(plugin_key),
                batch::text(account_id),
            ],
        );
        let operation_id = format!("account-pace-{what}-{}", uuid::Uuid::new_v4());
        self.execute_host_batch(batch::request(
            &operation_id,
            vec![batch::exec(update, update_params), insert],
        ))
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use chrono::{DateTime, Utc};

    use sea_orm::EntityTrait;

    use super::{DownloadId, DownloadStarter};
    use crate::pacing::PaceOverride;
    use crate::test_support::{sqlite, Profile};

    const KEY: &str = "plugin:audible";

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000 + secs, 0).expect("in range")
    }

    /// The window counts the last 24 hours of downloads, oldest first, and
    /// recording one prunes rows older than two days.
    #[tokio::test]
    async fn the_ledger_counts_the_last_day_and_prunes_older_rows() {
        for profile in Profile::ALL {
            let store = sqlite::memory(profile).await.into_store();
            store
                .upsert_account(KEY, "a1", "us", None, true, "audible")
                .await
                .unwrap();
            for (secs, starter) in [
                (0, DownloadStarter::Host),
                (90_000, DownloadStarter::Person),
                (95_000, DownloadStarter::Host),
            ] {
                store
                    .record_account_download(
                        &DownloadId::new(),
                        KEY,
                        "a1",
                        at(secs),
                        Some("job-1"),
                        starter,
                    )
                    .await
                    .unwrap();
            }
            let state = store
                .account_pace_state(KEY, "a1", at(100_000))
                .await
                .unwrap();
            assert_eq!(state.window.starts, vec![at(90_000), at(95_000)]);
            // Three days later the first row is pruned by the next record.
            store
                .record_account_download(
                    &DownloadId::new(),
                    KEY,
                    "a1",
                    at(3 * 86_400),
                    None,
                    DownloadStarter::Host,
                )
                .await
                .unwrap();
            let rows = crate::entities::account_downloads::Entity::find()
                .all(&store.db)
                .await
                .unwrap();
            assert_eq!(rows.len(), 3, "{profile:?}");
        }
    }

    /// The window is read in pages of the database's result cap, so a
    /// one-row cap still sees every download, including two that started
    /// in the same millisecond.
    #[tokio::test]
    async fn the_window_reads_in_pages_of_the_result_cap() {
        for profile in Profile::ALL {
            let store = sqlite::memory(profile)
                .await
                .into_store()
                .with_plan_limits(64, 1, 65_536);
            assert_eq!(store.db_capabilities().max_result_rows, 1);
            store
                .upsert_account(KEY, "a1", "us", None, true, "audible")
                .await
                .unwrap();
            for secs in [90_000, 90_000, 95_000] {
                store
                    .record_account_download(
                        &DownloadId::new(),
                        KEY,
                        "a1",
                        at(secs),
                        None,
                        DownloadStarter::Host,
                    )
                    .await
                    .unwrap();
            }
            let state = store
                .account_pace_state(KEY, "a1", at(100_000))
                .await
                .unwrap();
            assert_eq!(
                state.window.starts,
                vec![at(90_000), at(90_000), at(95_000)],
                "{profile:?}"
            );
        }
    }

    /// Writing one download again with the same identity, as a retry after
    /// a lost answer does, keeps one ledger row.
    #[tokio::test]
    async fn a_retried_write_of_one_download_keeps_one_row() {
        for profile in Profile::ALL {
            let store = sqlite::memory(profile).await.into_store();
            store
                .upsert_account(KEY, "a1", "us", None, true, "audible")
                .await
                .unwrap();
            let id = DownloadId::new();
            for _ in 0..2 {
                store
                    .record_account_download(&id, KEY, "a1", at(0), None, DownloadStarter::Host)
                    .await
                    .unwrap();
            }
            let state = store.account_pace_state(KEY, "a1", at(1)).await.unwrap();
            assert_eq!(state.window.count(), 1, "{profile:?}");
        }
    }

    /// A download for an account the library never recorded is ignored,
    /// not an error.
    #[tokio::test]
    async fn an_unknown_account_records_nothing() {
        let store = sqlite::memory(Profile::ALL[0]).await.into_store();
        store
            .record_account_download(
                &DownloadId::new(),
                KEY,
                "ghost",
                at(0),
                None,
                DownloadStarter::Host,
            )
            .await
            .unwrap();
        store
            .set_account_pace_override(KEY, "ghost", &PaceOverride::Unlimited)
            .await
            .unwrap();
        let state = store.account_pace_state(KEY, "ghost", at(1)).await.unwrap();
        assert_eq!(state.window.count(), 0);
        assert_eq!(state.override_, PaceOverride::Default);
    }

    /// The override round-trips, a later cool-down replaces an earlier
    /// one but never the other way, and clearing it keeps the override.
    #[tokio::test]
    async fn the_override_and_cooldown_are_kept_apart() {
        for profile in Profile::ALL {
            let store = sqlite::memory(profile).await.into_store();
            store
                .upsert_account(KEY, "a1", "us", None, true, "audible")
                .await
                .unwrap();
            store
                .extend_account_cooldown(KEY, "a1", at(500))
                .await
                .unwrap();
            let custom = PaceOverride::Custom {
                downloads_per_day: NonZeroU32::new(5),
                min_gap_seconds: Some(60),
            };
            store
                .set_account_pace_override(KEY, "a1", &custom)
                .await
                .unwrap();
            store
                .extend_account_cooldown(KEY, "a1", at(100))
                .await
                .unwrap();
            let state = store.account_pace_state(KEY, "a1", at(0)).await.unwrap();
            assert_eq!(state.override_, custom, "{profile:?}");
            assert_eq!(state.cooldown_until, Some(at(500)));
            store
                .extend_account_cooldown(KEY, "a1", at(900))
                .await
                .unwrap();
            let state = store.account_pace_state(KEY, "a1", at(0)).await.unwrap();
            assert_eq!(state.cooldown_until, Some(at(900)));
            store.clear_account_cooldown(KEY, "a1").await.unwrap();
            let state = store.account_pace_state(KEY, "a1", at(0)).await.unwrap();
            assert_eq!(state.cooldown_until, None);
            assert_eq!(state.override_, custom);
            store
                .set_account_pace_override(KEY, "a1", &PaceOverride::Default)
                .await
                .unwrap();
            let state = store.account_pace_state(KEY, "a1", at(0)).await.unwrap();
            assert_eq!(state.override_, PaceOverride::Default);
        }
    }

    /// An override out of range is refused before anything is written.
    #[tokio::test]
    async fn an_out_of_range_override_is_refused() {
        let store = sqlite::memory(Profile::ALL[0]).await.into_store();
        store
            .upsert_account(KEY, "a1", "us", None, true, "audible")
            .await
            .unwrap();
        let err = store
            .set_account_pace_override(
                KEY,
                "a1",
                &PaceOverride::Custom {
                    downloads_per_day: NonZeroU32::new(20_000),
                    min_gap_seconds: None,
                },
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("downloads_per_day"), "{err}");
    }
}
