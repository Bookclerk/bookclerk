//! Hash-based scan deltas for storefronts with no change feed.
//!
//! A store such as Libro.fm, Chirp or GraphicAudio can only list a whole
//! library, so every pass fetches every store page. [`ScanDelta`] keeps a
//! fingerprint of each title the last pass listed in the plugin's own KV
//! namespace and lets the plugin send the host only the titles that are new
//! or changed, plus the product ids that disappeared. The store requests are
//! the same; the host receives, compares and writes far less.
//!
//! # How a pass uses it
//!
//! 1. An empty `syncToken` is a full pass. Every title goes to the host, and
//!    the pass records a snapshot of fingerprints and product ids.
//! 2. The pass's last page returns a token naming that snapshot
//!    (`hd1.<generation>`). The host sends it on the account's next pass.
//! 3. A pass with a token is a delta pass. Each page sends only titles whose
//!    fingerprint the snapshot does not hold. The last page lists the
//!    snapshot's product ids that no page of the pass listed as `removed`,
//!    records the new snapshot and returns its token. A delta pass is never
//!    `complete`.
//!
//! # State it keeps
//!
//! Keys live under `hd1/<account hash>/`:
//!
//! - `p/<generation>/<store page hash>`: the product ids and fingerprints one
//!   store page of the pass in progress listed (JSON).
//! - `s/<generation>/fp`: the sorted fingerprints of a finished pass
//!   (8 bytes each, big-endian).
//! - `s/<generation>/ids`: the product ids of a finished pass (JSON).
//!
//! The generation of a pass is derived from the account and the token the
//! pass started with, so every call of one pass, resumed or not, agrees on
//! it without keeping state. The call that starts a pass (empty cursor)
//! deletes the account's earlier pending pages and every snapshot but the
//! one its token names. A snapshot is replaced only by a later pass, so a
//! host that lost the last page of a pass and sends the older token again
//! still finds the snapshot it needs.
//!
//! # When it falls back
//!
//! - No KV binding (the operator has not approved it yet): every pass is a
//!   full pass with an empty token, as before.
//! - A token this module did not write, or one whose snapshot is gone:
//!   `invalid_cursor`, so the host restarts once as a full pass.
//! - A snapshot or a store page too large for one KV value
//!   ([`MAX_KV_VALUE_BYTES`], about 130,000 titles of fingerprints): the
//!   pass ends with an empty token, so the next pass is a full pass.

use std::collections::{BTreeSet, HashSet};

use bookclerk_plugin_abi::{
    KvListOptions, KvNamespace, KvPutOptions, PluginError, ScanBook, ScanParams, MAX_KV_VALUE_BYTES,
};
use sha2::{Digest, Sha256};

/// `[[kv_namespaces]]` binding first-party storefronts declare for this
/// module's state.
pub const SCAN_STATE_BINDING: &str = "SCAN_STATE";

/// Prefix of every key and token this module writes; bumps when the layout
/// changes, which turns every stored token into `invalid_cursor`.
const LAYOUT: &str = "hd1";

/// Domain separator hashed into every fingerprint. Changing it (or the
/// `ScanBook` wire shape) makes every title read as changed once, which is
/// safe: the host just compares and writes them.
const FINGERPRINT_DOMAIN: &[u8] = b"bookclerk-scan-delta-fingerprint-v1\0";

/// Domain separator for the generation a pass derives from its token.
const GENERATION_DOMAIN: &[u8] = b"bookclerk-scan-delta-generation-v1\0";

/// Domain separator for the account and store page segments of a key.
const KEY_DOMAIN: &[u8] = b"bookclerk-scan-delta-key-v1\0";

/// Seconds a pending store page lives: long enough for a resumed pass (the
/// host resumes a cursor up to 24 hours old), short enough that an
/// abandoned pass cleans itself up.
const PENDING_TTL_SECONDS: u64 = 2 * 24 * 60 * 60;

/// Seconds a snapshot lives. The host asks for a full pass at least every
/// seven days, so a snapshot this old is never the base of a delta.
const SNAPSHOT_TTL_SECONDS: u64 = 21 * 24 * 60 * 60;

/// Keys one `list` call asks for.
const LIST_LIMIT: u32 = 1000;

/// Bytes one fingerprint takes in a snapshot.
const FINGERPRINT_BYTES: usize = 8;

/// Fingerprint of one title: the first 8 bytes of SHA-256 over its wire
/// form, so any field the host stores changes it.
type Fingerprint = u64;

/// What one store page of a pass sends to the host.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeltaPage {
    /// Titles to send: all of them on a full pass, the new or changed ones
    /// on a delta pass.
    pub books: Vec<ScanBook>,
    /// On the last store page of a delta pass: product ids the snapshot held
    /// and no page of this pass listed.
    pub removed: Vec<String>,
    /// On the last store page: the token for the account's next pass, empty
    /// when the next pass must be a full pass.
    pub sync_token: String,
    /// On the last store page: true for a full pass, which listed every
    /// title the account holds.
    pub complete: bool,
}

/// One store page's record in the pass in progress.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct PendingPage {
    /// Product ids the store page listed, in store order.
    ids: Vec<String>,
    /// Fingerprints of those titles, in the same order.
    fps: Vec<Fingerprint>,
}

/// Hash-based delta state for one call of one account's pass.
///
/// Build it with [`ScanDelta::open`] at the top of `scan`, then pass every
/// store page through [`ScanDelta::page`].
pub struct ScanDelta<'a> {
    /// The plugin's KV namespace; `None` turns every pass into a plain full
    /// pass.
    kv: Option<&'a dyn KvNamespace>,
    /// `hd1/<account hash>`: the prefix of every key of the account.
    account: String,
    /// Generation of the snapshot the pass compares against; `None` on a
    /// full pass.
    base: Option<String>,
    /// Generation this pass records.
    generation: String,
    /// Fingerprints of the base snapshot, sorted; empty on a full pass.
    base_fps: Vec<Fingerprint>,
}

impl<'a> ScanDelta<'a> {
    /// Reads the pass's token and, on the call that starts a pass, clears
    /// the account's stale state.
    ///
    /// # Arguments
    ///
    /// * `kv` - The plugin's scan-state namespace, when the operator granted
    ///   it.
    /// * `params` - The scan call's params (account, cursor, sync token).
    ///
    /// # Errors
    ///
    /// Returns `invalid_cursor` for a token this module did not write or
    /// whose snapshot is gone, so the host restarts as a full pass, and the
    /// namespace's error when a KV call fails.
    pub async fn open(
        kv: Option<&'a dyn KvNamespace>,
        params: &ScanParams,
    ) -> Result<Self, PluginError> {
        let account = format!("{LAYOUT}/{}", key_segment(params.account_id.as_bytes()));
        let generation = generation(&params.account_id, &params.sync_token);
        let base = if params.sync_token.is_empty() {
            None
        } else {
            Some(parse_token(&params.sync_token)?)
        };
        let mut delta = Self {
            kv,
            account,
            base,
            generation,
            base_fps: Vec::new(),
        };
        let Some(kv) = delta.kv else {
            if delta.base.is_some() {
                // A token from when the namespace was granted: start over.
                return Err(stale_token());
            }
            return Ok(delta);
        };
        if params.cursor.is_empty() {
            delta.clear_stale(kv).await?;
        }
        if let Some(base) = delta.base.clone() {
            let found = kv.get(delta.snapshot_key(&base, "fp")).await?;
            if !found.found {
                return Err(stale_token());
            }
            delta.base_fps = decode_fingerprints(&found.value).ok_or_else(stale_token)?;
        }
        Ok(delta)
    }

    /// Filters one store page and, on the last one, ends the pass.
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
    /// Returns `invalid_cursor` when the base snapshot's product ids are gone
    /// at the end of a delta pass, and the namespace's error when a KV call
    /// fails.
    pub async fn page(
        &self,
        store_cursor: &str,
        books: Vec<ScanBook>,
        last: bool,
    ) -> Result<DeltaPage, PluginError> {
        let full = self.base.is_none();
        let Some(kv) = self.kv else {
            return Ok(DeltaPage {
                books,
                complete: last,
                ..DeltaPage::default()
            });
        };
        let record = PendingPage {
            ids: books.iter().map(|b| b.product_id.clone()).collect(),
            fps: books.iter().map(fingerprint).collect(),
        };
        let books = if full {
            books
        } else {
            books
                .into_iter()
                .zip(&record.fps)
                .filter(|(_, fp)| self.base_fps.binary_search(fp).is_err())
                .map(|(book, _)| book)
                .collect()
        };
        let pending = serde_json::to_vec(&record)
            .map_err(|err| PluginError::internal(format!("encode scan delta page: {err}")))?;
        let page_key = format!(
            "{}{}",
            self.pending_prefix(),
            key_segment(store_cursor.as_bytes())
        );
        if fits(&pending) {
            put(kv, page_key, pending, PENDING_TTL_SECONDS).await?;
        } else {
            put(kv, self.overflow_key(), Vec::new(), PENDING_TTL_SECONDS).await?;
        }
        let mut page = DeltaPage {
            books,
            ..DeltaPage::default()
        };
        if last {
            self.finish(kv, &mut page).await?;
        }
        Ok(page)
    }

    /// Ends the pass on its last store page: works out `removed`, records the
    /// new snapshot and sets the token.
    ///
    /// # Errors
    ///
    /// Returns `invalid_cursor` when the base snapshot's ids are gone, and
    /// the namespace's error when a KV call fails.
    async fn finish(&self, kv: &dyn KvNamespace, page: &mut DeltaPage) -> Result<(), PluginError> {
        page.complete = self.base.is_none();
        let mut ids = BTreeSet::new();
        let mut fps = BTreeSet::new();
        let mut overflow = false;
        for key in list_keys(kv, &self.pending_prefix()).await? {
            if key == self.overflow_key() {
                overflow = true;
                continue;
            }
            let found = kv.get(key).await?;
            let Some(record) = found
                .found
                .then(|| serde_json::from_slice::<PendingPage>(&found.value).ok())
                .flatten()
            else {
                // A page record expired or unreadable: this pass cannot
                // vouch for a snapshot, so the next one is full.
                overflow = true;
                continue;
            };
            ids.extend(record.ids);
            fps.extend(record.fps);
        }
        if let Some(base) = &self.base {
            let found = kv.get(self.snapshot_key(base, "ids")).await?;
            let base_ids: Vec<String> = found
                .found
                .then(|| serde_json::from_slice(&found.value).ok())
                .flatten()
                .ok_or_else(stale_token)?;
            page.removed = base_ids
                .into_iter()
                .filter(|id| !ids.contains(id))
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
        }
        if overflow {
            return Ok(());
        }
        let ids_value = serde_json::to_vec(&ids)
            .map_err(|err| PluginError::internal(format!("encode scan delta ids: {err}")))?;
        let fps_value = encode_fingerprints(&fps);
        if !fits(&ids_value) || !fits(&fps_value) {
            return Ok(());
        }
        // Ids first: a pass that finds `fp` also finds `ids`, or restarts.
        put(
            kv,
            self.snapshot_key(&self.generation, "ids"),
            ids_value,
            SNAPSHOT_TTL_SECONDS,
        )
        .await?;
        put(
            kv,
            self.snapshot_key(&self.generation, "fp"),
            fps_value,
            SNAPSHOT_TTL_SECONDS,
        )
        .await?;
        page.sync_token = format!("{LAYOUT}.{}", self.generation);
        Ok(())
    }

    /// Deletes the account's pending pages and every snapshot but the base.
    ///
    /// # Errors
    ///
    /// Returns the namespace's error when a KV call fails.
    async fn clear_stale(&self, kv: &dyn KvNamespace) -> Result<(), PluginError> {
        let keep = self
            .base
            .as_ref()
            .map(|base| format!("{}/s/{base}/", self.account));
        for key in list_keys(kv, &format!("{}/", self.account)).await? {
            if keep.as_ref().is_some_and(|keep| key.starts_with(keep)) {
                continue;
            }
            kv.delete(key).await?;
        }
        Ok(())
    }

    /// Prefix of this pass's pending store pages.
    fn pending_prefix(&self) -> String {
        format!("{}/p/{}/", self.account, self.generation)
    }

    /// Key marking that a store page of this pass was too large to record.
    fn overflow_key(&self) -> String {
        format!("{}overflow", self.pending_prefix())
    }

    /// Key of one part (`fp` or `ids`) of a snapshot.
    fn snapshot_key(&self, generation: &str, part: &str) -> String {
        format!("{}/s/{generation}/{part}", self.account)
    }
}

/// The error a token this module cannot use gets, so the host restarts the
/// account with a full pass.
fn stale_token() -> PluginError {
    PluginError::invalid_cursor("scan sync token is not one this plugin can use")
}

/// Generation a token names.
///
/// # Errors
///
/// Returns `invalid_cursor` for a token this module did not write.
fn parse_token(token: &str) -> Result<String, PluginError> {
    token
        .strip_prefix(LAYOUT)
        .and_then(|rest| rest.strip_prefix('.'))
        .filter(|g| g.len() == 32 && g.bytes().all(|b| b.is_ascii_hexdigit()))
        .filter(|g| g.bytes().all(|b| !b.is_ascii_uppercase()))
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

/// Key segment for an account id or store cursor: 16 hex digits of a hash,
/// so keys stay short and never carry the account id itself.
fn key_segment(raw: &[u8]) -> String {
    let mut hash = Sha256::new();
    hash.update(KEY_DOMAIN);
    hash.update(raw);
    hex(&hash.finalize()[..8])
}

/// Fingerprint of a title's wire form.
fn fingerprint(book: &ScanBook) -> Fingerprint {
    let mut hash = Sha256::new();
    hash.update(FINGERPRINT_DOMAIN);
    // Serializing a generated struct of strings, numbers and lists cannot
    // fail; an empty body would only make the title read as changed.
    hash.update(serde_json::to_vec(book).unwrap_or_default());
    let digest = hash.finalize();
    let mut first = [0_u8; FINGERPRINT_BYTES];
    first.copy_from_slice(&digest[..FINGERPRINT_BYTES]);
    u64::from_be_bytes(first)
}

/// Snapshot value of sorted fingerprints.
fn encode_fingerprints(fps: &BTreeSet<Fingerprint>) -> Vec<u8> {
    fps.iter().flat_map(|fp| fp.to_be_bytes()).collect()
}

/// Sorted fingerprints a snapshot value holds; `None` when the value is not
/// one [`encode_fingerprints`] wrote.
fn decode_fingerprints(value: &[u8]) -> Option<Vec<Fingerprint>> {
    let (chunks, rest) = value.as_chunks::<FINGERPRINT_BYTES>();
    if !rest.is_empty() {
        return None;
    }
    let fps: Vec<Fingerprint> = chunks.iter().map(|c| u64::from_be_bytes(*c)).collect();
    fps.windows(2).all(|w| w[0] < w[1]).then_some(fps)
}

/// Whether a value fits one KV value.
fn fits(value: &[u8]) -> bool {
    u64::try_from(value.len()).is_ok_and(|len| len <= u64::from(MAX_KV_VALUE_BYTES))
}

/// Writes one key with a TTL.
///
/// # Errors
///
/// Returns the namespace's error.
async fn put(
    kv: &dyn KvNamespace,
    key: String,
    value: Vec<u8>,
    ttl_seconds: u64,
) -> Result<(), PluginError> {
    kv.put(
        key,
        value,
        KvPutOptions {
            expiration_ttl_seconds: ttl_seconds,
            ..KvPutOptions::default()
        },
    )
    .await
    .map(|_| ())
}

/// Every key under `prefix`, following list pages.
///
/// # Errors
///
/// Returns the namespace's error, or `internal` when the namespace repeats
/// a list cursor.
async fn list_keys(kv: &dyn KvNamespace, prefix: &str) -> Result<Vec<String>, PluginError> {
    let mut keys = Vec::new();
    let mut cursor = String::new();
    let mut seen = HashSet::new();
    loop {
        let page = kv
            .list(KvListOptions {
                prefix: prefix.to_owned(),
                cursor: cursor.clone(),
                limit: LIST_LIMIT,
            })
            .await?;
        keys.extend(page.keys.into_iter().map(|k| k.key));
        if page.list_complete || page.cursor.is_empty() {
            return Ok(keys);
        }
        if !seen.insert(page.cursor.clone()) {
            return Err(PluginError::internal("KV list repeated a cursor"));
        }
        cursor = page.cursor;
    }
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
    use std::cell::RefCell;
    use std::collections::BTreeMap;

    use bookclerk_plugin_abi::{KvDeleteOk, KvGetOk, KvListKey, KvListOk, KvPutOk};

    use super::*;

    /// In-memory namespace that lists two keys per page, so paging is
    /// exercised.
    #[derive(Default)]
    struct MemoryKv {
        /// Stored values by key.
        rows: RefCell<BTreeMap<String, Vec<u8>>>,
    }

    #[async_trait::async_trait(?Send)]
    impl KvNamespace for MemoryKv {
        async fn get(&self, key: String) -> Result<KvGetOk, PluginError> {
            let value = self.rows.borrow().get(&key).cloned();
            Ok(KvGetOk {
                found: value.is_some(),
                value: value.unwrap_or_default(),
                ..KvGetOk::default()
            })
        }

        async fn put(
            &self,
            key: String,
            value: Vec<u8>,
            _options: KvPutOptions,
        ) -> Result<KvPutOk, PluginError> {
            self.rows.borrow_mut().insert(key, value);
            Ok(KvPutOk::default())
        }

        async fn delete(&self, key: String) -> Result<KvDeleteOk, PluginError> {
            self.rows.borrow_mut().remove(&key);
            Ok(KvDeleteOk::default())
        }

        async fn list(&self, options: KvListOptions) -> Result<KvListOk, PluginError> {
            let rows = self.rows.borrow();
            let mut keys: Vec<String> = rows
                .keys()
                .filter(|k| k.starts_with(&options.prefix) && **k > options.cursor)
                .cloned()
                .collect();
            let list_complete = keys.len() <= 2;
            keys.truncate(2);
            Ok(KvListOk {
                cursor: if list_complete {
                    String::new()
                } else {
                    keys.last().cloned().unwrap_or_default()
                },
                keys: keys
                    .into_iter()
                    .map(|key| KvListKey {
                        key,
                        ..KvListKey::default()
                    })
                    .collect(),
                list_complete,
            })
        }
    }

    fn book(id: &str, title: &str) -> ScanBook {
        ScanBook {
            product_id: id.into(),
            title: title.into(),
            ..ScanBook::default()
        }
    }

    fn params(cursor: &str, token: &str) -> ScanParams {
        ScanParams {
            account_id: "a1".into(),
            cursor: cursor.into(),
            sync_token: token.into(),
            ..ScanParams::default()
        }
    }

    /// Runs one pass over `pages` of the store and returns every page's
    /// answer.
    async fn pass(kv: &MemoryKv, token: &str, pages: &[Vec<ScanBook>]) -> Vec<DeltaPage> {
        let mut out = Vec::new();
        for (i, books) in pages.iter().enumerate() {
            let cursor = if i == 0 {
                String::new()
            } else {
                format!("sp1.0.{i}")
            };
            let delta = ScanDelta::open(Some(kv), &params(&cursor, token))
                .await
                .unwrap();
            let store_cursor = if i == 0 { String::new() } else { i.to_string() };
            out.push(
                delta
                    .page(&store_cursor, books.clone(), i + 1 == pages.len())
                    .await
                    .unwrap(),
            );
        }
        out
    }

    fn ids(pages: &[DeltaPage]) -> Vec<String> {
        pages
            .iter()
            .flat_map(|p| p.books.iter().map(|b| b.product_id.clone()))
            .collect()
    }

    #[tokio::test]
    async fn a_full_pass_sends_everything_and_returns_a_token() {
        let kv = MemoryKv::default();
        let pages = pass(
            &kv,
            "",
            &[vec![book("1", "A"), book("2", "B")], vec![book("3", "C")]],
        )
        .await;
        assert_eq!(ids(&pages), ["1", "2", "3"]);
        let last = pages.last().unwrap();
        assert!(last.complete);
        assert!(last.removed.is_empty());
        assert!(last.sync_token.starts_with("hd1."));
        assert!(pages[0].sync_token.is_empty());
    }

    #[tokio::test]
    async fn a_delta_pass_sends_only_changes_and_lists_removals() {
        let kv = MemoryKv::default();
        let first = pass(
            &kv,
            "",
            &[vec![book("1", "A"), book("2", "B")], vec![book("3", "C")]],
        )
        .await;
        let token = first.last().unwrap().sync_token.clone();
        // "1" is unchanged but moved pages, "2" changed, "3" is gone, "4" is new.
        let second = pass(
            &kv,
            &token,
            &[vec![book("4", "D"), book("2", "B2")], vec![book("1", "A")]],
        )
        .await;
        assert_eq!(ids(&second), ["4", "2"]);
        let last = second.last().unwrap();
        assert!(!last.complete);
        assert_eq!(last.removed, ["3"]);
        assert_ne!(last.sync_token, token);
        // Nothing changed since: an empty delta that still moves the token on.
        let third = pass(
            &kv,
            &last.sync_token,
            &[vec![book("4", "D"), book("2", "B2"), book("1", "A")]],
        )
        .await;
        assert!(ids(&third).is_empty());
        assert!(third[0].removed.is_empty());
        assert!(third[0].sync_token.starts_with("hd1."));
    }

    #[tokio::test]
    async fn a_lost_last_page_can_be_replayed_with_the_older_token() {
        let kv = MemoryKv::default();
        let first = pass(&kv, "", &[vec![book("1", "A")]]).await;
        let token = first[0].sync_token.clone();
        let lost = pass(&kv, &token, &[vec![book("1", "A2")]]).await;
        // The host never stored `lost`'s token and sends the older one again.
        let again = pass(&kv, &token, &[vec![book("1", "A2")]]).await;
        assert_eq!(ids(&again), ["1"]);
        assert_eq!(again[0].sync_token, lost[0].sync_token);
    }

    #[tokio::test]
    async fn a_repeated_slice_gives_the_same_answer() {
        let kv = MemoryKv::default();
        let first = pass(&kv, "", &[vec![book("1", "A"), book("2", "B")]]).await;
        let token = first[0].sync_token.clone();
        let delta = ScanDelta::open(Some(&kv), &params("", &token))
            .await
            .unwrap();
        let once = delta.page("", vec![book("1", "A1")], true).await.unwrap();
        let resumed = ScanDelta::open(Some(&kv), &params("sp1.500.", &token))
            .await
            .unwrap();
        let twice = resumed.page("", vec![book("1", "A1")], true).await.unwrap();
        assert_eq!(once, twice);
        assert_eq!(once.removed, ["2"]);
    }

    #[tokio::test]
    async fn foreign_or_forgotten_tokens_restart_the_pass() {
        let kv = MemoryKv::default();
        for token in [
            "page:2",
            "hd1.",
            "hd1.xyz",
            "hd2.00000000000000000000000000000000",
            "hd1.0000000000000000000000000000000A",
            "hd1.00000000000000000000000000000000",
        ] {
            let err = ScanDelta::open(Some(&kv), &params("", token))
                .await
                .err()
                .expect(token);
            assert_eq!(
                err.code,
                bookclerk_plugin_abi::PluginErrorCode::InvalidCursor
            );
        }
        let err = ScanDelta::open(None, &params("", "hd1.00000000000000000000000000000000"))
            .await
            .err()
            .expect("a token without the namespace");
        assert_eq!(
            err.code,
            bookclerk_plugin_abi::PluginErrorCode::InvalidCursor
        );
    }

    #[tokio::test]
    async fn a_full_pass_clears_older_state() {
        let kv = MemoryKv::default();
        let first = pass(&kv, "", &[vec![book("1", "A")], vec![book("2", "B")]]).await;
        let token = first.last().unwrap().sync_token.clone();
        let delta = pass(&kv, &token, &[vec![book("1", "A")]]).await;
        let delta_token = delta[0].sync_token.clone();
        // A later full pass keeps only its own page and snapshot.
        let full = pass(&kv, "", &[vec![book("1", "A")]]).await;
        assert!(full[0].complete);
        assert_eq!(full[0].sync_token, token);
        let keys: Vec<String> = kv.rows.borrow().keys().cloned().collect();
        assert_eq!(keys.len(), 3, "{keys:?}");
        // The delta snapshot it cleared is gone, so that token restarts.
        let err = ScanDelta::open(Some(&kv), &params("", &delta_token))
            .await
            .err()
            .expect("cleared snapshot");
        assert_eq!(
            err.code,
            bookclerk_plugin_abi::PluginErrorCode::InvalidCursor
        );
    }

    #[tokio::test]
    async fn without_a_namespace_every_pass_is_a_plain_full_pass() {
        let delta = ScanDelta::open(None, &params("", "")).await.unwrap();
        let page = delta.page("", vec![book("1", "A")], true).await.unwrap();
        assert_eq!(ids(std::slice::from_ref(&page)), ["1"]);
        assert!(page.complete && page.sync_token.is_empty());
    }

    #[test]
    fn fingerprints_round_trip_and_refuse_other_bytes() {
        let fps: BTreeSet<Fingerprint> = [3, 1, 2].into_iter().collect();
        assert_eq!(
            decode_fingerprints(&encode_fingerprints(&fps)),
            Some(vec![1, 2, 3])
        );
        assert_eq!(decode_fingerprints(&[0; 7]), None);
        let unsorted: Vec<u8> = [2_u64, 1].iter().flat_map(|f| f.to_be_bytes()).collect();
        assert_eq!(decode_fingerprints(&unsorted), None);
        assert_ne!(fingerprint(&book("1", "A")), fingerprint(&book("1", "B")));
    }
}
