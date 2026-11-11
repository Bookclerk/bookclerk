//! Storefront scan state per account: sync tokens and pass checkpoints.
//!
//! A scan pass walks one account's library page by page
//! (`ContentSource.scan`, `docs/plugins/abi.md#paged-scans`). After the host writes a
//! page it checkpoints the next cursor here, so a scan job that stops part
//! way resumes from that page; the last page stores the plugin's opaque sync
//! token for the next pass. The host decides from this row whether the next
//! pass may be a delta ([`plan_pass`]).
//!
//! Every write is one atomic host batch: `UPDATE` then `INSERT OR IGNORE`,
//! because canonical SQL-v1 has no `ON CONFLICT … DO UPDATE`.

use chrono::{DateTime, Duration, Utc};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

use super::{parse_dt, parse_dt_opt, LibraryStore};
use crate::control_plane::batch;
use crate::entities::account_scan_state;
use crate::error::{LibraryError, Result};

/// Longest time since the last complete pass before the host asks for a full
/// pass again, so a store whose deltas miss edits or removals is corrected.
pub const FULL_PASS_INTERVAL: Duration = Duration::days(7);

/// Longest time a stopped pass's cursor is resumed; an older one restarts
/// the pass (store cursors expire).
pub const RESUME_CURSOR_TTL: Duration = Duration::hours(24);

/// Where one account's scan stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanState {
    /// Opaque token from the end of the last pass; empty for none.
    pub sync_token: String,
    /// Cursor of a stopped pass's next page; empty when none.
    pub resume_cursor: String,
    /// Token the stopped pass started with.
    pub resume_token: String,
    /// When the last complete pass ended.
    pub last_complete_at: Option<DateTime<Utc>>,
    /// When the row was last written.
    pub updated_at: DateTime<Utc>,
}

impl From<account_scan_state::Model> for ScanState {
    fn from(row: account_scan_state::Model) -> Self {
        Self {
            sync_token: row.sync_token,
            resume_cursor: row.resume_cursor,
            resume_token: row.resume_token,
            last_complete_at: parse_dt_opt(row.last_complete_at.as_deref()),
            updated_at: parse_dt(&row.updated_at),
        }
    }
}

/// How the next pass of one account starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PassStart {
    /// Cursor of the first page to ask for; empty starts at the beginning.
    pub cursor: String,
    /// Sync token to send; empty asks for a full pass.
    pub sync_token: String,
    /// True when the pass resumes one that stopped part way.
    pub resumed: bool,
}

/// Decides how the next pass of an account starts.
///
/// A stopped pass whose cursor is younger than [`RESUME_CURSOR_TTL`] resumes
/// with the token it started with. Otherwise a pass starts from the
/// beginning, as a delta from the stored token unless no complete pass ended
/// within [`FULL_PASS_INTERVAL`], in which case it is full.
///
/// # Arguments
///
/// * `state` - The stored row, `None` before the account's first pass.
/// * `now` - The current time.
#[must_use]
pub fn plan_pass(state: Option<&ScanState>, now: DateTime<Utc>) -> PassStart {
    let Some(state) = state else {
        return PassStart {
            cursor: String::new(),
            sync_token: String::new(),
            resumed: false,
        };
    };
    if !state.resume_cursor.is_empty() && now - state.updated_at <= RESUME_CURSOR_TTL {
        return PassStart {
            cursor: state.resume_cursor.clone(),
            sync_token: state.resume_token.clone(),
            resumed: true,
        };
    }
    let fresh = state
        .last_complete_at
        .is_some_and(|at| now - at <= FULL_PASS_INTERVAL);
    PassStart {
        cursor: String::new(),
        sync_token: if fresh {
            state.sync_token.clone()
        } else {
            String::new()
        },
        resumed: false,
    }
}

impl LibraryStore {
    /// Reads where an account's scan stands.
    ///
    /// # Errors
    ///
    /// Returns an error when the read fails.
    pub async fn scan_state(
        &self,
        plugin_key: &str,
        account_id: &str,
    ) -> Result<Option<ScanState>> {
        Ok(account_scan_state::Entity::find()
            .filter(account_scan_state::Column::PluginKey.eq(plugin_key))
            .filter(account_scan_state::Column::AccountId.eq(account_id))
            .one(&self.db)
            .await
            .map_err(LibraryError::Orm)?
            .map(ScanState::from))
    }

    /// Records that a pass wrote a page and continues at `cursor`.
    ///
    /// # Arguments
    ///
    /// * `plugin_key` - The account's plugin.
    /// * `account_id` - The account.
    /// * `cursor` - The next page's cursor.
    /// * `pass_token` - The sync token the pass started with.
    ///
    /// # Errors
    ///
    /// Returns an error when the batch fails.
    pub async fn save_scan_checkpoint(
        &self,
        plugin_key: &str,
        account_id: &str,
        cursor: &str,
        pass_token: &str,
    ) -> Result<()> {
        let now = super::now_str();
        self.write_scan_state(
            "UPDATE account_scan_state SET resume_cursor = ?, resume_token = ?, updated_at = ? \
             WHERE plugin_key = ? AND account_id = ?",
            vec![
                batch::text(cursor),
                batch::text(pass_token),
                batch::text(&now),
                batch::text(plugin_key),
                batch::text(account_id),
            ],
            InsertRow {
                plugin_key,
                account_id,
                sync_token: "",
                resume_cursor: cursor,
                resume_token: pass_token,
                last_complete_at: None,
                now: &now,
            },
        )
        .await
    }

    /// Records the end of a pass: stores the plugin's next sync token, drops
    /// the resume cursor, and stamps `last_complete_at` for a complete pass.
    ///
    /// # Errors
    ///
    /// Returns an error when the batch fails.
    pub async fn finish_scan_pass(
        &self,
        plugin_key: &str,
        account_id: &str,
        sync_token: &str,
        complete: bool,
    ) -> Result<()> {
        let now = super::now_str();
        let (sql, mut params) = if complete {
            (
                "UPDATE account_scan_state SET sync_token = ?, resume_cursor = '', \
                 resume_token = '', last_complete_at = ?, updated_at = ? \
                 WHERE plugin_key = ? AND account_id = ?",
                vec![batch::text(sync_token), batch::text(&now)],
            )
        } else {
            (
                "UPDATE account_scan_state SET sync_token = ?, resume_cursor = '', \
                 resume_token = '', updated_at = ? \
                 WHERE plugin_key = ? AND account_id = ?",
                vec![batch::text(sync_token)],
            )
        };
        params.extend([
            batch::text(&now),
            batch::text(plugin_key),
            batch::text(account_id),
        ]);
        self.write_scan_state(
            sql,
            params,
            InsertRow {
                plugin_key,
                account_id,
                sync_token,
                resume_cursor: "",
                resume_token: "",
                last_complete_at: complete.then_some(now.as_str()),
                now: &now,
            },
        )
        .await
    }

    /// Forgets an account's sync token and resume cursor (the plugin called
    /// one invalid), so its next pass is full and starts at the beginning.
    ///
    /// # Errors
    ///
    /// Returns an error when the batch fails.
    pub async fn reset_scan_state(&self, plugin_key: &str, account_id: &str) -> Result<()> {
        let now = super::now_str();
        self.write_scan_state(
            "UPDATE account_scan_state SET sync_token = '', resume_cursor = '', \
             resume_token = '', updated_at = ? WHERE plugin_key = ? AND account_id = ?",
            vec![
                batch::text(&now),
                batch::text(plugin_key),
                batch::text(account_id),
            ],
            InsertRow {
                plugin_key,
                account_id,
                sync_token: "",
                resume_cursor: "",
                resume_token: "",
                last_complete_at: None,
                now: &now,
            },
        )
        .await
    }

    /// Runs `update` and the matching insert of a missing row as one batch.
    async fn write_scan_state(
        &self,
        update: &str,
        update_params: Vec<bookclerk_plugin_abi::DbValue>,
        row: InsertRow<'_>,
    ) -> Result<()> {
        let insert = batch::exec(
            "INSERT OR IGNORE INTO account_scan_state \
             (plugin_key, account_id, sync_token, resume_cursor, resume_token, \
              last_complete_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
            vec![
                batch::text(row.plugin_key),
                batch::text(row.account_id),
                batch::text(row.sync_token),
                batch::text(row.resume_cursor),
                batch::text(row.resume_token),
                batch::text_opt(row.last_complete_at),
                batch::text(row.now),
            ],
        );
        let operation_id = format!("scan-state-{}", uuid::Uuid::new_v4());
        self.execute_host_batch(batch::request(
            &operation_id,
            vec![batch::exec(update, update_params), insert],
        ))
        .await?;
        Ok(())
    }
}

/// The row [`LibraryStore::write_scan_state`] inserts when none exists.
struct InsertRow<'a> {
    /// The account's plugin.
    plugin_key: &'a str,
    /// The account.
    account_id: &'a str,
    /// Sync token to store.
    sync_token: &'a str,
    /// Resume cursor to store.
    resume_cursor: &'a str,
    /// Resume token to store.
    resume_token: &'a str,
    /// `last_complete_at` to store.
    last_complete_at: Option<&'a str>,
    /// Write time.
    now: &'a str,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(cursor: &str, updated_hours_ago: i64, complete_days_ago: Option<i64>) -> ScanState {
        let now = Utc::now();
        ScanState {
            sync_token: "tok".into(),
            resume_cursor: cursor.into(),
            resume_token: "pass-tok".into(),
            last_complete_at: complete_days_ago.map(|d| now - Duration::days(d)),
            updated_at: now - Duration::hours(updated_hours_ago),
        }
    }

    #[test]
    fn a_first_pass_is_full() {
        let start = plan_pass(None, Utc::now());
        assert_eq!(start.cursor, "");
        assert_eq!(start.sync_token, "");
        assert!(!start.resumed);
    }

    #[test]
    fn a_fresh_stopped_pass_resumes_with_its_own_token() {
        let start = plan_pass(Some(&state("page:3", 1, Some(1))), Utc::now());
        assert_eq!(start.cursor, "page:3");
        assert_eq!(start.sync_token, "pass-tok");
        assert!(start.resumed);
    }

    #[test]
    fn a_stale_cursor_restarts_the_pass() {
        let start = plan_pass(Some(&state("page:3", 25, Some(1))), Utc::now());
        assert_eq!(start.cursor, "");
        assert_eq!(start.sync_token, "tok");
        assert!(!start.resumed);
    }

    #[test]
    fn deltas_need_a_recent_complete_pass() {
        assert_eq!(
            plan_pass(Some(&state("", 0, Some(6))), Utc::now()).sync_token,
            "tok"
        );
        assert_eq!(
            plan_pass(Some(&state("", 0, Some(8))), Utc::now()).sync_token,
            ""
        );
        assert_eq!(
            plan_pass(Some(&state("", 0, None)), Utc::now()).sync_token,
            ""
        );
    }

    /// Every write lands under each database profile, and removing the
    /// account removes its row.
    #[tokio::test]
    async fn writes_round_trip_under_every_profile() {
        use crate::test_support::{sqlite, Profile};
        for profile in Profile::ALL {
            let store = sqlite::memory(profile).await.into_store();
            store
                .upsert_account("plugin:audible", "a1", "us", None, true, "audible")
                .await
                .unwrap();
            assert_eq!(
                store.scan_state("plugin:audible", "a1").await.unwrap(),
                None
            );

            store
                .save_scan_checkpoint("plugin:audible", "a1", "page:2", "t0")
                .await
                .unwrap();
            let row = store
                .scan_state("plugin:audible", "a1")
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                (row.resume_cursor.as_str(), row.resume_token.as_str()),
                ("page:2", "t0"),
                "{profile:?}"
            );
            assert_eq!(row.sync_token, "");
            assert!(row.last_complete_at.is_none());

            store
                .finish_scan_pass("plugin:audible", "a1", "t1", true)
                .await
                .unwrap();
            let row = store
                .scan_state("plugin:audible", "a1")
                .await
                .unwrap()
                .unwrap();
            assert_eq!(row.sync_token, "t1");
            assert_eq!(row.resume_cursor, "");
            let completed = row.last_complete_at.expect("complete pass stamped");

            store
                .finish_scan_pass("plugin:audible", "a1", "t2", false)
                .await
                .unwrap();
            let row = store
                .scan_state("plugin:audible", "a1")
                .await
                .unwrap()
                .unwrap();
            assert_eq!(row.sync_token, "t2");
            assert_eq!(row.last_complete_at, Some(completed), "a delta keeps it");

            store
                .reset_scan_state("plugin:audible", "a1")
                .await
                .unwrap();
            let row = store
                .scan_state("plugin:audible", "a1")
                .await
                .unwrap()
                .unwrap();
            assert_eq!(row.sync_token, "");
            assert_eq!(row.last_complete_at, Some(completed));

            // A first write may also be the end of a pass or a reset.
            store
                .upsert_account("plugin:audible", "a2", "us", None, true, "audible")
                .await
                .unwrap();
            store
                .finish_scan_pass("plugin:audible", "a2", "t9", true)
                .await
                .unwrap();
            let row = store
                .scan_state("plugin:audible", "a2")
                .await
                .unwrap()
                .unwrap();
            assert_eq!(row.sync_token, "t9");
            assert!(row.last_complete_at.is_some());
            store
                .upsert_account("plugin:audible", "a3", "us", None, true, "audible")
                .await
                .unwrap();
            store
                .reset_scan_state("plugin:audible", "a3")
                .await
                .unwrap();
            assert!(store
                .scan_state("plugin:audible", "a3")
                .await
                .unwrap()
                .is_some());

            // Remapping an account drops the old id's row with the account,
            // so the new id starts with a full pass.
            store
                .remap_account_id("plugin:audible", "a1", "a9")
                .await
                .unwrap();
            assert_eq!(
                store.scan_state("plugin:audible", "a1").await.unwrap(),
                None
            );
            assert_eq!(
                store.scan_state("plugin:audible", "a9").await.unwrap(),
                None
            );
        }
    }
}
