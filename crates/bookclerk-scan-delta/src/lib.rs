//! Hash-based scan deltas for storefronts with no change feed.
//!
//! A store such as Libro.fm, Chirp or GraphicAudio can only list a whole
//! library, so every pass fetches every store page. [`ScanDelta`] keeps a
//! fingerprint of each title the last pass listed in the plugin's own
//! database (the [`SCAN_STATE_BINDING`] `[[databases]]` binding) and lets the
//! plugin send the host only the titles that are new or changed, plus the
//! product ids that disappeared. The store requests are the same; the host
//! receives, compares and writes far less.
//!
//! # How a pass uses it
//!
//! 1. An empty `syncToken` is a full pass. Every title goes to the host, and
//!    the pass records one row per title.
//! 2. The pass's last page marks the pass finished and returns a token
//!    naming it (`hd1.<generation>`). The host sends it on the account's
//!    next pass.
//! 3. A pass with a token is a delta pass. Each page sends only titles whose
//!    product id and fingerprint the named pass does not hold. The last page
//!    lists the named pass's product ids that no page of this pass listed as
//!    `removed`, marks this pass finished and returns its token. A delta pass
//!    is never `complete`.
//!
//! # State it keeps
//!
//! Two tables, created by [`migrations`]:
//!
//! - `scan_delta_titles`: one row per title a pass listed (account, pass
//!   generation, store page, product id, fingerprint).
//! - `scan_delta_passes`: one row per pass, finished or not, with the time it
//!   was last touched.
//!
//! The account and store page columns hold hashes, never the account id or
//! the store's cursor. The generation of a pass is derived from the account
//! and the token the pass started with, so every call of one pass, resumed
//! or not, agrees on it without keeping state. The call that starts a pass
//! (empty cursor) deletes the account's rows other than the pass its token
//! names, and every pass of any account untouched for
//! [`RETENTION_SECONDS`]. A finished pass is replaced only by a later pass,
//! so a host that lost the last page of a pass and sends the older token
//! again still finds the pass it needs.
//!
//! Every write is split with the binding's `chunk()` and every read is
//! sized from the session's limits (one streamed read when the session
//! streams, key-ordered pages when it does not), so the same code runs on
//! the shipped adapters, on D1 and on the smallest advertisement a host
//! accepts. See `docs/plugins/database-limits.md`.
//!
//! # When it falls back
//!
//! - No binding (the operator has not approved it yet): every pass is a full
//!   pass with an empty token, as before.
//! - A token this crate did not write, or one whose pass is gone or never
//!   finished: `invalid_cursor`, so the host restarts once as a full pass.
//! - A pass whose row was cleared while it ran (another pass of the account
//!   started): its last page returns `invalid_cursor` instead of removals
//!   worked out from a partial record, and the host starts over.

use std::collections::{HashMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

use bookclerk_plugin_sdk::{
    DatabaseBinding, DbValue, PluginError, PluginMigration, PluginMigrationOp, ScanBook, ScanParams,
};
use sha2::{Digest, Sha256};

/// `[[databases]]` binding first-party storefronts declare for this crate's
/// state.
pub const SCAN_STATE_BINDING: &str = "SCAN_STATE";

/// Seconds a pass is kept after it was last touched. The host asks for a
/// full pass at least every seven days, so an older pass is never the base
/// of a delta; this only reclaims accounts that stopped scanning.
pub const RETENTION_SECONDS: i64 = 21 * 24 * 60 * 60;

/// Prefix of every token this crate writes; bumps when the layout changes,
/// which turns every stored token into `invalid_cursor`.
const LAYOUT: &str = "hd1";

/// Domain separator hashed into every fingerprint. Changing it (or the
/// `ScanBook` wire shape) makes every title read as changed once, which is
/// safe: the host just compares and writes them.
const FINGERPRINT_DOMAIN: &[u8] = b"bookclerk-scan-delta-fingerprint-v1\0";

/// Domain separator for the generation a pass derives from its token.
const GENERATION_DOMAIN: &[u8] = b"bookclerk-scan-delta-generation-v1\0";

/// Domain separator for the account and store page columns.
const KEY_DOMAIN: &[u8] = b"bookclerk-scan-delta-key-v1\0";

/// The `database_migrations` registration for [`SCAN_STATE_BINDING`].
///
/// Applied once, in order, before the binding is handed to the plugin.
#[must_use]
pub fn migrations() -> Vec<PluginMigration> {
    vec![PluginMigration {
        id: "scan-delta-v1".into(),
        operations: vec![
            PluginMigrationOp::Schema(
                "CREATE TABLE IF NOT EXISTS scan_delta_titles (\
                 account TEXT NOT NULL, \
                 generation TEXT NOT NULL, \
                 page TEXT NOT NULL, \
                 product_id TEXT NOT NULL, \
                 fingerprint INTEGER NOT NULL, \
                 PRIMARY KEY (account, generation, product_id))"
                    .into(),
            ),
            PluginMigrationOp::Schema(
                "CREATE INDEX IF NOT EXISTS scan_delta_titles_page \
                 ON scan_delta_titles (account, generation, page, product_id)"
                    .into(),
            ),
            PluginMigrationOp::Schema(
                "CREATE TABLE IF NOT EXISTS scan_delta_passes (\
                 account TEXT NOT NULL, \
                 generation TEXT NOT NULL, \
                 finished INTEGER NOT NULL, \
                 touched_at INTEGER NOT NULL, \
                 PRIMARY KEY (account, generation))"
                    .into(),
            ),
            PluginMigrationOp::Schema(
                "CREATE INDEX IF NOT EXISTS scan_delta_passes_touched \
                 ON scan_delta_passes (touched_at)"
                    .into(),
            ),
        ],
    }]
}

/// The `database_migrations` answer for `binding`.
///
/// # Returns
///
/// [`migrations`] for [`SCAN_STATE_BINDING`], nothing for any other binding.
#[must_use]
pub fn migrations_for(binding: &str) -> Vec<PluginMigration> {
    if binding == SCAN_STATE_BINDING {
        migrations()
    } else {
        Vec::new()
    }
}

/// What one store page of a pass sends to the host.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeltaPage {
    /// Titles to send: all of them on a full pass, the new or changed ones
    /// on a delta pass.
    pub books: Vec<ScanBook>,
    /// On the last store page of a delta pass: product ids the named pass
    /// held and no page of this pass listed, sorted.
    pub removed: Vec<String>,
    /// On the last store page: the token for the account's next pass, empty
    /// when the next pass must be a full pass.
    pub sync_token: String,
    /// On the last store page: true for a full pass, which listed every
    /// title the account holds.
    pub complete: bool,
}

/// Hash-based delta state for one call of one account's pass.
///
/// Build it with [`ScanDelta::open`] at the top of `scan`, then pass every
/// store page through [`ScanDelta::page`].
pub struct ScanDelta<'a> {
    /// The plugin's scan-state database; `None` turns every pass into a
    /// plain full pass.
    db: Option<&'a DatabaseBinding>,
    /// Hash of the account id: the `account` column of every row.
    account: String,
    /// Generation of the pass this one compares against; `None` on a full
    /// pass.
    base: Option<String>,
    /// Generation this pass records.
    generation: String,
}

impl std::fmt::Debug for ScanDelta<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScanDelta")
            .field("granted", &self.db.is_some())
            .field("base", &self.base)
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

impl<'a> ScanDelta<'a> {
    /// Reads the pass's token and, on the call that starts a pass, clears
    /// the account's stale rows.
    ///
    /// # Arguments
    ///
    /// * `db` - The plugin's scan-state database, when the operator granted
    ///   it.
    /// * `params` - The scan call's params (account, cursor, sync token).
    ///
    /// # Errors
    ///
    /// Returns `invalid_cursor` for a token this crate did not write or
    /// whose pass is gone or unfinished, so the host restarts as a full
    /// pass, and the binding's error when a database call fails.
    pub async fn open(
        db: Option<&'a DatabaseBinding>,
        params: &ScanParams,
    ) -> Result<Self, PluginError> {
        let base = if params.sync_token.is_empty() {
            None
        } else {
            Some(parse_token(&params.sync_token)?)
        };
        let delta = Self {
            db,
            account: key_segment(params.account_id.as_bytes()),
            base,
            generation: generation(&params.account_id, &params.sync_token),
        };
        let Some(db) = delta.db else {
            if delta.base.is_some() {
                // A token from when the binding was granted: start over.
                return Err(stale_token());
            }
            return Ok(delta);
        };
        if params.cursor.is_empty() {
            delta.start(db).await?;
        }
        if let Some(base) = &delta.base {
            let found = db
                .prepare(
                    "SELECT finished FROM scan_delta_passes \
                     WHERE account = ? AND generation = ?",
                )
                .bind(vec![text(&delta.account), text(base)])
                .first(None)
                .await?;
            let finished = found
                .and_then(|row| row.into_iter().find(|(name, _)| name == "finished"))
                .is_some_and(|(_, value)| matches!(value, DbValue::Int64(1)));
            if !finished {
                return Err(stale_token());
            }
        }
        Ok(delta)
    }

    /// Records one store page and filters it; on the last one, ends the
    /// pass.
    ///
    /// The same store page may come through more than once (the SDK's
    /// `scan_page` helper fetches a large page again for each slice); each
    /// time gives the same answer.
    ///
    /// # Arguments
    ///
    /// * `store_cursor` - The plugin's own cursor for this store page (empty
    ///   for the first).
    /// * `books` - Every title the store page lists, in store order.
    /// * `last` - Whether this is the last store page of the pass.
    ///
    /// # Errors
    ///
    /// Returns `invalid_cursor` on the last store page when another pass of
    /// the account cleared this one while it ran, and the binding's error
    /// when a database call fails.
    pub async fn page(
        &self,
        store_cursor: &str,
        books: Vec<ScanBook>,
        last: bool,
    ) -> Result<DeltaPage, PluginError> {
        let Some(db) = self.db else {
            return Ok(DeltaPage {
                books,
                complete: last,
                ..DeltaPage::default()
            });
        };
        let page_key = key_segment(store_cursor.as_bytes());
        let inserts = books
            .iter()
            .map(|book| {
                db.prepare(
                    "INSERT OR IGNORE INTO scan_delta_titles \
                     (account, generation, page, product_id, fingerprint) \
                     VALUES (?, ?, ?, ?, ?)",
                )
                .bind(vec![
                    text(&self.account),
                    text(&self.generation),
                    text(&page_key),
                    text(&book.product_id),
                    DbValue::Int64(fingerprint(book)),
                ])
                .as_run()
            })
            .collect();
        for batch in db.chunk(inserts, None).await? {
            db.batch(batch, None).await?;
        }
        let books = match &self.base {
            None => books,
            Some(base) => {
                let changed: HashSet<String> = read_ids(
                    db,
                    "SELECT p.product_id AS product_id FROM scan_delta_titles p \
                     LEFT JOIN scan_delta_titles b \
                     ON b.account = p.account AND b.generation = ? \
                     AND b.product_id = p.product_id AND b.fingerprint = p.fingerprint \
                     WHERE p.account = ? AND p.generation = ? AND p.page = ? \
                     AND b.product_id IS NULL AND p.product_id > ? \
                     ORDER BY p.product_id LIMIT ?",
                    vec![
                        text(base),
                        text(&self.account),
                        text(&self.generation),
                        text(&page_key),
                    ],
                )
                .await?
                .into_iter()
                .collect();
                books
                    .into_iter()
                    .filter(|book| changed.contains(&book.product_id))
                    .collect()
            }
        };
        let mut page = DeltaPage {
            books,
            ..DeltaPage::default()
        };
        if last {
            self.finish(db, &mut page).await?;
        }
        Ok(page)
    }

    /// Starts a pass: drops the account's rows other than the base pass,
    /// every pass of any account untouched for [`RETENTION_SECONDS`], and
    /// records this pass as unfinished.
    ///
    /// # Errors
    ///
    /// Returns the binding's error.
    async fn start(&self, db: &DatabaseBinding) -> Result<(), PluginError> {
        let now = now_seconds();
        let keep = text(self.base.as_deref().unwrap_or_default());
        let expired = DbValue::Int64(now.saturating_sub(RETENTION_SECONDS));
        let statements = vec![
            db.prepare("DELETE FROM scan_delta_titles WHERE account = ? AND generation <> ?")
                .bind(vec![text(&self.account), keep.clone()])
                .as_run(),
            db.prepare("DELETE FROM scan_delta_passes WHERE account = ? AND generation <> ?")
                .bind(vec![text(&self.account), keep])
                .as_run(),
            db.prepare(
                "DELETE FROM scan_delta_titles WHERE generation IN \
                 (SELECT generation FROM scan_delta_passes WHERE touched_at < ?)",
            )
            .bind(vec![expired.clone()])
            .as_run(),
            db.prepare("DELETE FROM scan_delta_passes WHERE touched_at < ?")
                .bind(vec![expired])
                .as_run(),
            db.prepare(
                "INSERT OR IGNORE INTO scan_delta_passes \
                 (account, generation, finished, touched_at) VALUES (?, ?, 0, ?)",
            )
            .bind(vec![
                text(&self.account),
                text(&self.generation),
                DbValue::Int64(now),
            ])
            .as_run(),
        ];
        for batch in db.chunk(statements, None).await? {
            db.batch(batch, None).await?;
        }
        Ok(())
    }

    /// Ends the pass on its last store page: works out `removed`, marks the
    /// pass finished and sets the token.
    ///
    /// # Errors
    ///
    /// Returns `invalid_cursor` when another pass of the account cleared
    /// this one, and the binding's error.
    async fn finish(&self, db: &DatabaseBinding, page: &mut DeltaPage) -> Result<(), PluginError> {
        page.complete = self.base.is_none();
        // Another pass of the account that started meanwhile deleted this
        // pass's row and the titles it had recorded, so `removed` worked out
        // now would name titles the account still holds.
        let live = db
            .prepare(
                "SELECT finished FROM scan_delta_passes \
                 WHERE account = ? AND generation = ?",
            )
            .bind(vec![text(&self.account), text(&self.generation)])
            .first(None)
            .await?;
        if live.is_none() {
            return Err(cleared_pass());
        }
        if let Some(base) = &self.base {
            page.removed = read_ids(
                db,
                "SELECT b.product_id AS product_id FROM scan_delta_titles b \
                 LEFT JOIN scan_delta_titles p \
                 ON p.account = b.account AND p.generation = ? \
                 AND p.product_id = b.product_id \
                 WHERE b.account = ? AND b.generation = ? \
                 AND p.product_id IS NULL AND b.product_id > ? \
                 ORDER BY b.product_id LIMIT ?",
                vec![text(&self.generation), text(&self.account), text(base)],
            )
            .await?;
        }
        let marked = db
            .prepare(
                "UPDATE scan_delta_passes SET finished = 1, touched_at = ? \
                 WHERE account = ? AND generation = ?",
            )
            .bind(vec![
                DbValue::Int64(now_seconds()),
                text(&self.account),
                text(&self.generation),
            ])
            .run(None)
            .await?;
        // No row: another pass cleared this one while `removed` was being
        // read, so that list cannot be trusted either.
        if marked.meta.changes == 0 {
            return Err(cleared_pass());
        }
        page.sync_token = format!("{LAYOUT}.{}", self.generation);
        Ok(())
    }
}

/// Every `product_id` a key-ordered query returns.
///
/// [`read_keyed`] keyed on `product_id`, keeping only that column.
///
/// # Errors
///
/// Returns what [`read_keyed`] returns, and `internal` for a row without a
/// text `product_id`.
async fn read_ids(
    db: &DatabaseBinding,
    sql: &str,
    binds: Vec<DbValue>,
) -> Result<Vec<String>, PluginError> {
    read_keyed(db, sql, binds, "product_id")
        .await?
        .iter()
        .map(product_id)
        .collect()
}

/// Every row a key-ordered query returns, read within the session's limits.
///
/// `sql` ends `AND <key> > ? ORDER BY <key> LIMIT ?`, where `<key>` is the
/// unique text column named by `key`, which the query also returns; `binds`
/// fills the placeholders before those two. When the session streams, one read with no
/// lower bound and no row limit returns it all. When it does not, every read
/// is held to the inline caps, so the rows come in pages keyed on `key`
/// (never `OFFSET`), each sized from `maxResultRows`, halved whenever a
/// page's bytes pass `maxResultBytes` and doubled again (up to
/// `maxResultRows`) after each full page.
///
/// # Arguments
///
/// * `db` - The plugin's database binding.
/// * `sql` - The query, shaped as above.
/// * `binds` - Values for the placeholders before the key bound and limit.
/// * `key` - Name of the returned text column the rows are ordered on.
///
/// # Returns
///
/// The rows in key order, each as column name to value.
///
/// # Errors
///
/// Returns the binding's error, `limit_exceeded` included when a single row
/// passes the inline caps, and `internal` when a paged row has no text
/// `key` column.
pub async fn read_keyed(
    db: &DatabaseBinding,
    sql: &str,
    binds: Vec<DbValue>,
    key: &str,
) -> Result<Vec<HashMap<String, DbValue>>, PluginError> {
    let caps = db.limits().await?;
    let mut out = Vec::new();
    if caps.streams_results() {
        let mut all = binds;
        all.extend([text(""), DbValue::Int64(i64::MAX)]);
        let mut rows = db.prepare(sql).bind(all).iterate(None);
        while let Some(row) = rows.next().await {
            out.push(row?);
        }
        return Ok(out);
    }
    let max_page = usize::try_from(caps.max_result_rows)
        .unwrap_or(usize::MAX)
        .max(1);
    let mut page_size = max_page;
    let mut after = String::new();
    loop {
        let mut page_binds = binds.clone();
        page_binds.extend([
            text(&after),
            DbValue::Int64(i64::try_from(page_size).unwrap_or(i64::MAX)),
        ]);
        let rows = match db.prepare(sql).bind(page_binds).all(None).await {
            Ok(read) => read.results.unwrap_or_default(),
            Err(err) => {
                let wide = err
                    .limit()
                    .is_some_and(|limit| limit.limit == "maxResultBytes");
                if wide && page_size > 1 {
                    page_size /= 2;
                    continue;
                }
                return Err(err);
            }
        };
        let full = rows.len() >= page_size;
        for row in rows {
            match row.get(key) {
                Some(DbValue::Text(id)) => after.clone_from(id),
                _ => {
                    return Err(PluginError::internal(format!(
                        "keyed read row has no text {key}"
                    )))
                }
            }
            out.push(row);
        }
        if !full {
            return Ok(out);
        }
        page_size = page_size.saturating_mul(2).min(max_page);
    }
}

/// The `product_id` column of one row.
///
/// # Errors
///
/// Returns `internal` when the column is missing or not text.
fn product_id(row: &HashMap<String, DbValue>) -> Result<String, PluginError> {
    match row.get("product_id") {
        Some(DbValue::Text(id)) => Ok(id.clone()),
        _ => Err(PluginError::internal(
            "scan delta row has no text product_id",
        )),
    }
}

/// A text bind value.
fn text(value: &str) -> DbValue {
    DbValue::Text(value.to_owned())
}

/// Seconds since the Unix epoch; `0` if the clock reads before it.
fn now_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_secs()).ok())
        .unwrap_or(0)
}

/// The error the last page of a pass gets when another pass of the account
/// cleared it while it ran, so the host restarts the account instead of
/// applying removals worked out from a partial record.
fn cleared_pass() -> PluginError {
    PluginError::invalid_cursor("another scan of this account replaced this one; start over")
}

/// The error a token this crate cannot use gets, so the host restarts the
/// account with a full pass.
fn stale_token() -> PluginError {
    PluginError::invalid_cursor("scan sync token is not one this plugin can use")
}

/// Generation a token names.
///
/// # Errors
///
/// Returns `invalid_cursor` for a token this crate did not write.
fn parse_token(token: &str) -> Result<String, PluginError> {
    token
        .strip_prefix(LAYOUT)
        .and_then(|rest| rest.strip_prefix('.'))
        .filter(|g| g.len() == 32)
        .filter(|g| {
            g.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
        .map(str::to_owned)
        .ok_or_else(stale_token)
}

/// Generation a pass records: a hash of the account and the token the pass
/// started with, 32 lowercase hex digits.
fn generation(account_id: &str, sync_token: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(GENERATION_DOMAIN);
    hash.update(account_id.as_bytes());
    hash.update([0]);
    hash.update(sync_token.as_bytes());
    hex(&hash.finalize()[..16])
}

/// Column value for an account id or store cursor: 16 hex digits of a hash,
/// so rows stay short and never carry the account id itself.
fn key_segment(raw: &[u8]) -> String {
    let mut hash = Sha256::new();
    hash.update(KEY_DOMAIN);
    hash.update(raw);
    hex(&hash.finalize()[..8])
}

/// Fingerprint of a title's wire form: the first 8 bytes of SHA-256, read
/// as a signed integer so every database stores it as `INTEGER`.
fn fingerprint(book: &ScanBook) -> i64 {
    let mut hash = Sha256::new();
    hash.update(FINGERPRINT_DOMAIN);
    // Serializing a generated struct of strings, numbers and lists cannot
    // fail; an empty body would only make the title read as changed.
    hash.update(serde_json::to_vec(book).unwrap_or_default());
    let digest = hash.finalize();
    let mut first = [0_u8; 8];
    first.copy_from_slice(&digest[..8]);
    i64::from_be_bytes(first)
}

/// Lowercase hex of `bytes`.
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_parse_only_in_the_shape_this_crate_writes() {
        let generation = generation("a1", "");
        assert_eq!(
            parse_token(&format!("hd1.{generation}")).unwrap(),
            generation
        );
        for token in [
            "page:2",
            "hd1.",
            "hd1.xyz",
            "hd2.00000000000000000000000000000000",
            "hd1.0000000000000000000000000000000A",
            "hd1.000000000000000000000000000000000",
        ] {
            assert!(parse_token(token).is_err(), "{token}");
        }
    }

    #[test]
    fn fingerprints_follow_every_field_and_columns_hide_the_account() {
        let book = |title: &str| ScanBook {
            product_id: "1".into(),
            title: title.into(),
            ..ScanBook::default()
        };
        assert_ne!(fingerprint(&book("A")), fingerprint(&book("B")));
        assert_eq!(fingerprint(&book("A")), fingerprint(&book("A")));
        let column = key_segment(b"account@example.com");
        assert_eq!(column.len(), 16);
        assert!(!column.contains("account"));
    }

    #[test]
    fn the_migrations_answer_for_the_scan_state_binding_only() {
        assert_eq!(migrations_for(SCAN_STATE_BINDING).len(), 1);
        assert!(migrations_for("OTHER").is_empty());
    }

    #[test]
    fn the_clock_reads_after_the_epoch() {
        assert!(now_seconds() > 1_700_000_000);
    }
}
