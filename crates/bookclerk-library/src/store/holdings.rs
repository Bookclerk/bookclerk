//! Holdings a holder loses: titles a scan no longer lists, accounts that
//! are revoked, account ids that are remapped, and titles a branch no
//! longer shares.
//!
//! A holding is added by the library model's link batch
//! ([`super::library_model`]) with a `holding_added` event. This module is
//! the other half: every removal deletes the row and publishes
//! `holding_removed` in the same atomic batch, so the search index and
//! other consumers hear about each one exactly once.
//!
//! Scans mark what they list: each written page stamps the pass id
//! (`account_scan_state.pass_id`) on the account's holdings of the page's
//! products ([`LibraryStore::mark_holdings_seen`]). When a complete full
//! pass ends, the account's holdings that no page stamped are the titles it
//! no longer holds ([`HoldingSelection::UnseenBy`]). A delta pass instead
//! names the products that left the account
//! ([`HoldingSelection::Products`]). A branch's titles are node holdings,
//! removed the same way when its catalog pass no longer names them or it
//! stops sharing ([`LibraryStore::remove_node_holdings`]).
//!
//! Removals read the matching holdings in keyset pages and then delete
//! each one under a guard that repeats the selection, so a holding that a
//! concurrent pass stamped, or another writer already removed, is left
//! alone and its event is not published.

use sea_orm::sea_query::Query;
use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter, QueryOrder, QuerySelect};
use uuid::Uuid;

use super::write_batches::WriteBatches;
use super::LibraryStore;
use crate::control_plane::{batch, LiveTopic};
use crate::entities::{holdings, store_listings};
use crate::error::{LibraryError, Result};

/// Event type published when a holding is removed.
pub const HOLDING_REMOVED: &str = "holding_removed";

/// Event type of a new holding (the link batch publishes most of them).
const HOLDING_ADDED: &str = "holding_added";

/// Most holdings one keyset page of a removal reads.
const REMOVAL_PAGE: u64 = 200;

/// `product_ids` in the spelling `store_listings.product_id` keeps, so a
/// guest's padded id meets its listing.
fn stored_ids(product_ids: &[String]) -> Vec<String> {
    let mut ids: Vec<String> = product_ids
        .iter()
        .map(|id| bookclerk_resolve::stored_product_id(id))
        .collect();
    ids.sort();
    ids.dedup();
    ids
}

/// Splits `ids` into chunks of at most `width`, halving a chunk until
/// `fits` admits its statement, so long ids (UUIDs on D1) stay under the
/// backend's `maxPayloadBytes` where a bind count alone would not.
pub(crate) fn fitting_chunks(
    ids: &[String],
    width: usize,
    fits: impl Fn(&[String]) -> bool,
) -> Vec<&[String]> {
    let mut out = Vec::new();
    let mut width = width.max(1);
    let mut rest = ids;
    while !rest.is_empty() {
        let mut n = width.min(rest.len());
        while n > 1 && !fits(&rest[..n]) {
            n /= 2;
        }
        let (part, tail) = rest.split_at(n);
        out.push(part);
        width = n;
        rest = tail;
    }
    out
}

/// Which holdings of one account a removal takes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HoldingSelection<'a> {
    /// The account's holdings of these products of its plugin (a scan page's
    /// `removed` ids).
    Products(&'a [String]),
    /// The account's holdings that the scan pass `pass_id` did not list.
    UnseenBy(&'a str),
    /// Every holding of the account (it was revoked).
    All,
}

/// Which holdings of one branch (node) a removal takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeHoldingSelection<'a> {
    /// The node's holdings its catalog pass `pass` (the node's
    /// `catalog_pass_started_at`) did not name, while that pass is still
    /// the node's and `fence` still its catalog fence: a connection that
    /// resumes the pass keeps what it links, and ends the pass itself.
    UnseenBy {
        /// The catalog pass id.
        pass: &'a str,
        /// The catalog fence of the batch that ended the pass.
        fence: i64,
    },
    /// Every holding of the node (it left or was revoked).
    All,
    /// Every holding of the node while `fence` is still the node's catalog
    /// fence (it stopped sharing under that fence): a newer connection that
    /// shares again keeps what it links.
    AllUnderFence(i64),
    /// These holdings of the node, by id (ones a title's newer holding
    /// replaced), while `fence` is still the node's catalog fence: a newer
    /// connection may have taken them up again.
    Replaced {
        /// The holdings' ids.
        ids: &'a [String],
        /// The catalog fence the replacing batch was written under.
        fence: i64,
    },
    /// The node's holdings created before this stamp that still carry no
    /// share state: ones a catalog batch linked before finding its fence
    /// gone. A holding a live batch has given its share state since stays,
    /// and one newer than the stamp may belong to a batch still in flight.
    UnsharedBefore(&'a str),
}

/// What each delete of a removal repeats, so a holding that changed since
/// it was read is kept.
#[derive(Debug, Clone, Copy)]
enum DeleteGuard<'a> {
    /// Only the holding's id.
    Id,
    /// The holding is still unseen by this account scan.
    UnseenBy(&'a str),
    /// The holding is still unseen by this catalog pass, which is still
    /// the node's, under this catalog fence.
    UnseenUnder {
        /// The catalog pass id.
        pass: &'a str,
        /// The node's expected catalog fence.
        fence: i64,
    },
    /// The holding still carries no share state.
    Unshared,
    /// The node's catalog fence is still this one.
    LiveFence(i64),
}

/// A read of `holdings` rows that leaves `share_title` out (read as
/// `NULL`): a branch's shared title may be larger than one cell a database
/// plugin may return (`maxCellBytes`), so it is read only on its own, in
/// pieces ([`share_title_of`]).
pub(crate) fn find_holdings() -> sea_orm::Select<holdings::Entity> {
    use sea_orm::sea_query::Expr;
    use sea_orm::Iterable as _;
    let mut select = holdings::Entity::find().select_only();
    for column in holdings::Column::iter().filter(|c| !matches!(c, holdings::Column::ShareTitle)) {
        select = select.column(column);
    }
    select.column_as(Expr::cust("NULL"), "share_title")
}

/// One piece of a holding's `share_title`.
#[derive(Debug, sea_orm::FromQueryResult)]
struct SharePiece {
    /// The piece's text; `None` when the holding or its title is gone.
    piece: Option<String>,
    /// The holding's `updated_at` when the piece was read, which every
    /// share-state rewrite advances (nanosecond RFC 3339 text).
    updated_at: String,
}

/// Reads holding `holding_id`'s `share_title` in pieces that each fit one
/// cell the database plugin may return, so a title of any accepted size is
/// readable under every profile. Every piece is read only while the
/// holding's `share_digest` is still `digest`, the one the caller checked,
/// and its `updated_at` is still the one the first piece was read with.
///
/// # Returns
///
/// The title as the branch shared it, or `None` when the holding or its
/// title is gone, or the holding was rewritten while it was read.
///
/// # Errors
///
/// Returns the read error.
pub(crate) async fn share_title_of(
    store: &LibraryStore,
    holding_id: &str,
    digest: &str,
) -> Result<Option<String>> {
    use sea_orm::sea_query::Expr;
    // `substr` counts characters; a character is at most four bytes.
    let cell = match store.db_capabilities().max_cell_bytes {
        0 => 64 * 1024,
        cap => usize::try_from(cap).unwrap_or(usize::MAX),
    };
    let chars = i64::try_from((cell.saturating_sub(64) / 4).max(1)).unwrap_or(i64::MAX);
    let mut out: Option<String> = None;
    let mut version: Option<String> = None;
    let mut start: i64 = 1;
    loop {
        let mut query = holdings::Entity::find()
            .select_only()
            .column_as(
                Expr::cust_with_values("substr(share_title, ?, ?)", [start, chars]),
                "piece",
            )
            .column(holdings::Column::UpdatedAt)
            .filter(holdings::Column::Id.eq(holding_id))
            .filter(holdings::Column::ShareDigest.eq(digest));
        if let Some(version) = &version {
            query = query.filter(holdings::Column::UpdatedAt.eq(version.as_str()));
        }
        let row = query
            .into_model::<SharePiece>()
            .one(store.db())
            .await
            .map_err(LibraryError::Orm)?;
        // Every piece requires the digest the caller read and the
        // `updated_at` of the first piece. The digest leaves metadata out,
        // so a rewrite with the same digest still advances `updated_at`:
        // a title rewritten (or removed) part way is never stitched from
        // two versions.
        let Some(SharePiece {
            piece: Some(piece),
            updated_at,
        }) = row
        else {
            return Ok(None);
        };
        version.get_or_insert(updated_at);
        let read = i64::try_from(piece.chars().count()).unwrap_or(i64::MAX);
        out.get_or_insert_with(String::new).push_str(&piece);
        if read < chars {
            return Ok(out);
        }
        start = start.saturating_add(read);
    }
}

/// A listing's id and plugin, the source a node holding's removal event
/// names.
#[derive(Debug, sea_orm::FromQueryResult)]
struct ListingSource {
    /// The listing (`store_listings.id`).
    id: String,
    /// The plugin that sells it.
    plugin_key: String,
}

/// Whose holdings a removal reads: one account, or one branch node.
#[derive(Debug, Clone, Copy)]
enum Scope<'a> {
    /// A store account, by plugin key and account id.
    Account {
        /// The account's plugin.
        plugin_key: &'a str,
        /// The account id.
        account_id: &'a str,
    },
    /// A branch library, by node id.
    Node(&'a str),
}

impl Scope<'_> {
    /// The condition that a holding is this scope's.
    fn condition(self) -> Condition {
        match self {
            Self::Account {
                plugin_key,
                account_id,
            } => Condition::all()
                .add(holdings::Column::PluginKey.eq(plugin_key))
                .add(holdings::Column::AccountId.eq(account_id)),
            Self::Node(node_id) => Condition::all()
                .add(holdings::Column::HolderKind.eq("node"))
                .add(holdings::Column::NodeId.eq(node_id)),
        }
    }
}

/// One holding read for removal.
#[derive(Debug, Clone)]
struct Doomed {
    /// `holdings.id`.
    id: String,
    /// The listing held.
    listing_id: String,
    /// Stored holder key.
    holder: String,
    /// The event's `account_id` (empty for a node holding).
    account_id: String,
    /// The event's `source`: the listing's plugin key.
    source: String,
}

impl LibraryStore {
    /// Stamps scan pass `pass_id` on the account's holdings of `product_ids`.
    ///
    /// A holding is stamped whether or not its listing resolved on this
    /// page, so a title the library model could not settle is never taken
    /// for one that left the account.
    ///
    /// # Arguments
    ///
    /// * `plugin_key` - The account's plugin (and the listings').
    /// * `account_id` - The scanned account.
    /// * `pass_id` - The pass in progress (`account_scan_state.pass_id`).
    /// * `product_ids` - The product ids the page listed.
    ///
    /// # Errors
    ///
    /// Returns an error when a batch fails.
    pub async fn mark_holdings_seen(
        &self,
        plugin_key: &str,
        account_id: &str,
        pass_id: &str,
        product_ids: &[String],
    ) -> Result<()> {
        if product_ids.is_empty() || pass_id.is_empty() {
            return Ok(());
        }
        let product_ids = stored_ids(product_ids);
        let stamp = |chunk: &[String]| {
            let marks = vec!["?"; chunk.len()].join(", ");
            let mut params = vec![
                batch::text(pass_id),
                batch::text(plugin_key),
                batch::text(account_id),
                batch::text(plugin_key),
            ];
            params.extend(chunk.iter().map(|id| batch::text(id)));
            batch::exec(
                &format!(
                    "UPDATE holdings SET seen_pass = ? \
                     WHERE plugin_key = ? AND account_id = ? AND listing_id IN \
                     (SELECT id FROM store_listings WHERE plugin_key = ? AND product_id IN ({marks}))"
                ),
                params,
            )
        };
        let mut batches = WriteBatches::new(self.db_capabilities(), "holding stamp");
        for chunk in fitting_chunks(&product_ids, self.lookup_chunk(), |part| {
            self.admits_statement(&stamp(part))
        }) {
            if let Some(full) = batches.push(stamp(chunk))? {
                self.run_holdings_batch("holdings-seen", full).await?;
            }
        }
        if let Some(last) = batches.finish() {
            self.run_holdings_batch("holdings-seen", last).await?;
        }
        Ok(())
    }

    /// Removes the account's holdings that `selection` names, publishing
    /// `holding_removed` for each in the batch that deletes it.
    ///
    /// # Arguments
    ///
    /// * `plugin_key` - The account's plugin.
    /// * `account_id` - The account.
    /// * `selection` - Which of its holdings go.
    ///
    /// # Returns
    ///
    /// How many holdings were selected for removal (a concurrent writer may
    /// have kept or removed some of them first).
    ///
    /// # Errors
    ///
    /// Returns an error when a read or a batch fails; holdings removed by
    /// earlier batches stay removed.
    pub async fn remove_account_holdings(
        &self,
        plugin_key: &str,
        account_id: &str,
        selection: HoldingSelection<'_>,
    ) -> Result<usize> {
        let mut total = 0;
        match selection {
            HoldingSelection::Products(ids) => {
                let ids = stored_ids(ids);
                let of_products = |chunk: &[String]| {
                    let listings = Query::select()
                        .column(store_listings::Column::Id)
                        .from(store_listings::Entity)
                        .and_where(store_listings::Column::PluginKey.eq(plugin_key))
                        .and_where(store_listings::Column::ProductId.is_in(chunk.iter().cloned()))
                        .to_owned();
                    Condition::all().add(holdings::Column::ListingId.in_subquery(listings))
                };
                let scope = Scope::Account {
                    plugin_key,
                    account_id,
                };
                let fits = |part: &[String]| {
                    crate::control_plane::sea_exec(&self.selected_page(
                        scope,
                        "",
                        of_products(part),
                    ))
                    .map(|stmt| self.admits_read(&stmt))
                    .unwrap_or(true)
                };
                for chunk in fitting_chunks(&ids, self.lookup_chunk(), fits) {
                    total += self
                        .remove_selected(scope, of_products(chunk), DeleteGuard::Id)
                        .await?;
                }
            }
            HoldingSelection::UnseenBy(pass_id) => {
                let cond = Condition::any()
                    .add(holdings::Column::SeenPass.is_null())
                    .add(holdings::Column::SeenPass.ne(pass_id));
                total += self
                    .remove_selected(
                        Scope::Account {
                            plugin_key,
                            account_id,
                        },
                        cond,
                        DeleteGuard::UnseenBy(pass_id),
                    )
                    .await?;
            }
            HoldingSelection::All => {
                total += self
                    .remove_selected(
                        Scope::Account {
                            plugin_key,
                            account_id,
                        },
                        Condition::all(),
                        DeleteGuard::Id,
                    )
                    .await?;
            }
        }
        Ok(total)
    }

    /// Removes the holdings of branch `node_id` that `selection` names,
    /// publishing `holding_removed` for each in the batch that deletes it.
    /// Files already delivered for them stay with their editions.
    ///
    /// # Returns
    ///
    /// How many holdings were selected for removal.
    ///
    /// # Errors
    ///
    /// Returns an error when a read or a batch fails; holdings removed by
    /// earlier batches stay removed.
    pub async fn remove_node_holdings(
        &self,
        node_id: &str,
        selection: NodeHoldingSelection<'_>,
    ) -> Result<usize> {
        match selection {
            NodeHoldingSelection::UnseenBy { pass, fence } => {
                let cond = Condition::any()
                    .add(holdings::Column::SeenPass.is_null())
                    .add(holdings::Column::SeenPass.ne(pass));
                self.remove_selected(
                    Scope::Node(node_id),
                    cond,
                    DeleteGuard::UnseenUnder { pass, fence },
                )
                .await
            }
            NodeHoldingSelection::All => {
                self.remove_selected(Scope::Node(node_id), Condition::all(), DeleteGuard::Id)
                    .await
            }
            NodeHoldingSelection::AllUnderFence(fence) => {
                self.remove_selected(
                    Scope::Node(node_id),
                    Condition::all(),
                    DeleteGuard::LiveFence(fence),
                )
                .await
            }
            NodeHoldingSelection::Replaced { ids, fence } => {
                let of_ids = |part: &[String]| {
                    Condition::all().add(holdings::Column::Id.is_in(part.iter().cloned()))
                };
                let fits = |part: &[String]| {
                    crate::control_plane::sea_exec(&self.selected_page(
                        Scope::Node(node_id),
                        "",
                        of_ids(part),
                    ))
                    .map(|stmt| self.admits_read(&stmt))
                    .unwrap_or(true)
                };
                let mut total = 0;
                for chunk in fitting_chunks(ids, self.lookup_chunk(), fits) {
                    total += self
                        .remove_selected(
                            Scope::Node(node_id),
                            of_ids(chunk),
                            DeleteGuard::LiveFence(fence),
                        )
                        .await?;
                }
                Ok(total)
            }
            NodeHoldingSelection::UnsharedBefore(stamp) => {
                let cond = Condition::all()
                    .add(holdings::Column::ShareDigest.is_null())
                    .add(holdings::Column::CreatedAt.lt(stamp));
                self.remove_selected(Scope::Node(node_id), cond, DeleteGuard::Unshared)
                    .await
            }
        }
    }

    /// Reads the scope's holdings matching `cond` in keyset pages and
    /// removes each; `guard` is repeated in each delete.
    async fn remove_selected(
        &self,
        scope: Scope<'_>,
        cond: Condition,
        guard: DeleteGuard<'_>,
    ) -> Result<usize> {
        let mut after = String::new();
        let mut total = 0;
        loop {
            let rows = self
                .selected_page(scope, &after, cond.clone())
                .all(&self.db)
                .await
                .map_err(LibraryError::Orm)?;
            let Some(last) = rows.last() else {
                return Ok(total);
            };
            after.clone_from(&last.id);
            total += rows.len();
            let full = u64::try_from(rows.len()).unwrap_or(u64::MAX) >= self.removal_page();
            let page = self.doomed(scope, rows).await?;
            self.delete_holdings(scope, &page, guard).await?;
            if !full {
                return Ok(total);
            }
        }
    }

    /// The removal records of `rows`: an account holding's event names its
    /// account and plugin; a node holding's names no account and its
    /// listing's plugin, as the link batch's `holding_added` does.
    async fn doomed(&self, scope: Scope<'_>, rows: Vec<holdings::Model>) -> Result<Vec<Doomed>> {
        let sources: std::collections::BTreeMap<String, String> = match scope {
            Scope::Account { .. } => std::collections::BTreeMap::new(),
            Scope::Node(_) => {
                let ids: Vec<String> = rows.iter().map(|m| m.listing_id.clone()).collect();
                let query = |part: &[String]| {
                    store_listings::Entity::find()
                        .select_only()
                        .column(store_listings::Column::Id)
                        .column(store_listings::Column::PluginKey)
                        .filter(store_listings::Column::Id.is_in(part.iter().cloned()))
                };
                let fits = |part: &[String]| {
                    crate::control_plane::sea_exec(&query(part))
                        .map(|stmt| self.admits_read(&stmt))
                        .unwrap_or(true)
                };
                let mut out = std::collections::BTreeMap::new();
                for chunk in fitting_chunks(&ids, self.lookup_chunk(), fits) {
                    for listing in query(chunk)
                        .into_model::<ListingSource>()
                        .all(&self.db)
                        .await
                        .map_err(LibraryError::Orm)?
                    {
                        out.insert(listing.id, listing.plugin_key);
                    }
                }
                out
            }
        };
        Ok(rows
            .into_iter()
            .map(|m| {
                let (account_id, source) = match scope {
                    Scope::Account {
                        plugin_key,
                        account_id,
                    } => (account_id.to_string(), plugin_key.to_string()),
                    Scope::Node(_) => (
                        String::new(),
                        sources.get(&m.listing_id).cloned().unwrap_or_default(),
                    ),
                };
                Doomed {
                    id: m.id,
                    listing_id: m.listing_id,
                    holder: m.holder,
                    account_id,
                    source,
                }
            })
            .collect())
    }

    /// The read of one keyset page of the scope's holdings matching
    /// `cond`, after holding id `after`.
    fn selected_page(
        &self,
        scope: Scope<'_>,
        after: &str,
        cond: Condition,
    ) -> sea_orm::Select<holdings::Entity> {
        find_holdings()
            .filter(scope.condition())
            .filter(holdings::Column::Id.gt(after.to_string()))
            .filter(cond)
            .order_by_asc(holdings::Column::Id)
            .limit(self.removal_page())
    }

    /// Deletes `doomed`, each with its `holding_removed` event, in batches
    /// sized from the database plugin's limits. Each delete repeats
    /// `guard`: a pass guard for a node's catalog pass also requires the
    /// pass to still be the node's, so a pass started over meanwhile keeps
    /// what it stamps.
    async fn delete_holdings(
        &self,
        scope: Scope<'_>,
        doomed: &[Doomed],
        guard: DeleteGuard<'_>,
    ) -> Result<()> {
        let now = super::now_str();
        let mut batches = WriteBatches::new(self.db_capabilities(), "holding removal")
            .with_trailer(crate::control_plane::bump(LiveTopic::Library))?;
        for holding in doomed {
            let (guard, guard_binds) = match (guard, scope) {
                (
                    DeleteGuard::UnseenBy(pass) | DeleteGuard::UnseenUnder { pass, .. },
                    Scope::Account { .. },
                )
                | (DeleteGuard::UnseenBy(pass), Scope::Node(_)) => (
                    "id = ? AND (seen_pass IS NULL OR seen_pass <> ?)",
                    vec![batch::text(&holding.id), batch::text(pass)],
                ),
                (DeleteGuard::UnseenUnder { pass, fence }, Scope::Node(node_id)) => (
                    "id = ? AND (seen_pass IS NULL OR seen_pass <> ?) AND EXISTS \
                     (SELECT 1 FROM nodes WHERE node_id = ? AND catalog_pass_started_at = ? \
                       AND catalog_fence = ?)",
                    vec![
                        batch::text(&holding.id),
                        batch::text(pass),
                        batch::text(node_id),
                        batch::text(pass),
                        bookclerk_plugin_abi::DbValue::Int64(fence),
                    ],
                ),
                (DeleteGuard::Unshared, _) => (
                    "id = ? AND share_digest IS NULL",
                    vec![batch::text(&holding.id)],
                ),
                (DeleteGuard::LiveFence(fence), Scope::Node(node_id)) => (
                    "id = ? AND EXISTS \
                     (SELECT 1 FROM nodes WHERE node_id = ? AND catalog_fence = ?)",
                    vec![
                        batch::text(&holding.id),
                        batch::text(node_id),
                        bookclerk_plugin_abi::DbValue::Int64(fence),
                    ],
                ),
                (DeleteGuard::Id | DeleteGuard::LiveFence(_), _) => {
                    ("id = ?", vec![batch::text(&holding.id)])
                }
            };
            let event = holding_event(HOLDING_REMOVED, holding, &now, guard, guard_binds.clone());
            let delete = batch::exec(&format!("DELETE FROM holdings WHERE {guard}"), guard_binds);
            if let Some(full) = batches.push_group(vec![event, delete])? {
                self.run_holdings_batch("holdings-removed", full).await?;
            }
        }
        if let Some(last) = batches.finish() {
            self.run_holdings_batch("holdings-removed", last).await?;
        }
        Ok(())
    }

    /// Moves account `from`'s holdings to account `to` of the same plugin
    /// (an account id remap): each moves under a new id with
    /// `holding_removed` for the old holder and `holding_added` for the new
    /// one, and a holding `to` already has of the same listing is kept
    /// instead (the old one is removed).
    ///
    /// `to` must already be an account row, since the holding's foreign key
    /// names it.
    ///
    /// # Errors
    ///
    /// Returns an error when a read or a batch fails; holdings moved by
    /// earlier batches stay moved.
    pub(crate) async fn move_account_holdings(
        &self,
        plugin_key: &str,
        from: &str,
        to: &str,
    ) -> Result<usize> {
        let to_holder = format!("account:{to}");
        let mut total = 0;
        loop {
            let page = holdings::Entity::find()
                .filter(holdings::Column::PluginKey.eq(plugin_key))
                .filter(holdings::Column::AccountId.eq(from))
                .order_by_asc(holdings::Column::Id)
                .limit(self.removal_page())
                .all(&self.db)
                .await
                .map_err(LibraryError::Orm)?;
            if page.is_empty() {
                return Ok(total);
            }
            total += page.len();
            let now = super::now_str();
            let mut batches = WriteBatches::new(self.db_capabilities(), "holding move")
                .with_trailer(crate::control_plane::bump(LiveTopic::Library))?;
            for old in page {
                let doomed = Doomed {
                    id: old.id.clone(),
                    listing_id: old.listing_id.clone(),
                    holder: old.holder.clone(),
                    account_id: from.to_string(),
                    source: plugin_key.to_string(),
                };
                let new_id = Uuid::new_v4().to_string();
                let mut group = vec![holding_event(
                    HOLDING_REMOVED,
                    &doomed,
                    &now,
                    "id = ?",
                    vec![batch::text(&old.id)],
                )];
                group.push(batch::exec(
                    "UPDATE holdings SET id = ?, holder = ?, account_id = ?, updated_at = ? \
                     WHERE id = ? AND NOT EXISTS \
                     (SELECT 1 FROM holdings WHERE listing_id = ? AND holder = ?)",
                    vec![
                        batch::text(&new_id),
                        batch::text(&to_holder),
                        batch::text(to),
                        batch::text(&now),
                        batch::text(&old.id),
                        batch::text(&old.listing_id),
                        batch::text(&to_holder),
                    ],
                ));
                let moved = Doomed {
                    id: new_id.clone(),
                    listing_id: old.listing_id.clone(),
                    holder: to_holder.clone(),
                    account_id: to.to_string(),
                    source: plugin_key.to_string(),
                };
                group.push(holding_event(
                    HOLDING_ADDED,
                    &moved,
                    &now,
                    "id = ?",
                    vec![batch::text(&new_id)],
                ));
                group.push(batch::exec(
                    "DELETE FROM holdings WHERE id = ?",
                    vec![batch::text(&old.id)],
                ));
                if let Some(full) = batches.push_group(group)? {
                    self.run_holdings_batch("holdings-moved", full).await?;
                }
            }
            if let Some(last) = batches.finish() {
                self.run_holdings_batch("holdings-moved", last).await?;
            }
        }
    }

    /// Holdings one keyset page reads: [`REMOVAL_PAGE`], or fewer when the
    /// backend returns fewer rows per statement.
    fn removal_page(&self) -> u64 {
        u64::from(self.max_result_rows).clamp(1, REMOVAL_PAGE)
    }

    /// Sends one batch of holding writes.
    async fn run_holdings_batch(
        &self,
        prefix: &str,
        statements: Vec<bookclerk_plugin_abi::TypedDbStatement>,
    ) -> Result<()> {
        let operation_id = format!("{prefix}-{}", Uuid::new_v4());
        self.execute_host_batch(batch::request(&operation_id, statements))
            .await?;
        Ok(())
    }
}

/// The outbox insert of a holding event for `holding`, published only while
/// a holding matches `guard` (so a holding already gone, or kept by a
/// concurrent writer, announces nothing). The dedup key names the holding
/// id, which a holding never shares with an earlier one.
fn holding_event(
    event_type: &str,
    holding: &Doomed,
    now: &str,
    guard: &str,
    guard_binds: Vec<bookclerk_plugin_abi::DbValue>,
) -> bookclerk_plugin_abi::TypedDbStatement {
    let dedup_key = format!("{event_type}:{}", holding.id);
    let payload = serde_json::json!({
        "listingId": holding.listing_id,
        "holder": holding.holder,
        "holdingId": holding.id,
    })
    .to_string();
    let mut binds = vec![
        batch::text(&Uuid::new_v4().to_string()),
        batch::text(event_type),
        batch::text(now),
        batch::text(&holding.account_id),
        batch::text(&holding.source),
        batch::text(&holding.listing_id),
        batch::text(&dedup_key),
        batch::text(&payload),
        batch::text(&holding.listing_id),
        batch::text(now),
        batch::text(&holding.account_id),
        batch::text(&holding.source),
        batch::text(event_type),
        batch::text(&dedup_key),
    ];
    binds.extend(guard_binds);
    batch::exec(
        &format!(
            "INSERT OR IGNORE INTO domain_events (\
                id, event_type, schema_version, occurred_at, account_id, source, \
                correlation_id, causation_id, dedup_key, payload, ordering_key, \
                dispatch_state, created_at, wake_pending\
             ) SELECT ?, ?, 1, ?, ?, ?, ?, '', ?, ?, ?, 'pending', ?, 1 \
             WHERE NOT EXISTS (\
                SELECT 1 FROM domain_events \
                WHERE queue_name = 'core.events' AND account_id = ? AND source = ? \
                  AND event_type = ? AND dedup_key = ?\
             ) AND EXISTS (SELECT 1 FROM holdings WHERE {guard})"
        ),
        binds,
    )
}

#[cfg(test)]
mod tests {
    use bookclerk_resolve as resolve;
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder};

    use super::*;
    use crate::entities::{accounts, domain_events};
    use crate::test_support::{sqlite, Profile};

    /// The plugin every test listing is sold by.
    const PLUGIN: &str = "plugin:audible";

    /// An audiobook listing of `product`, titled after it.
    fn listing(product: &str) -> resolve::Listing {
        resolve::Listing::from_draft(&resolve::ListingDraft {
            plugin_key: PLUGIN.into(),
            marketplace: Some("us".into()),
            product_id: product.into(),
            work_kind: Some(resolve::WorkKind::Book),
            expression_kind: Some(resolve::ExpressionKind::Narration),
            media_kind: Some(resolve::MediaKind::Audiobook),
            title: format!("Title {product}"),
            identifiers: vec![(resolve::IdentifierScheme::Asin, product.into(), None)],
            ..resolve::ListingDraft::default()
        })
        .unwrap()
    }

    /// A purchase held by account `id`.
    fn held_by(id: &str) -> resolve::HoldingDraft {
        resolve::HoldingDraft {
            holder: resolve::Holder::Account(id.into()),
            entitlement: resolve::Entitlement::Purchase,
            purchased_at: None,
        }
    }

    /// A store under `profile` with account `a1` holding `products`.
    async fn library(profile: Profile, products: &[&str]) -> LibraryStore {
        let store = sqlite::memory(profile).await.into_store();
        store
            .upsert_account(PLUGIN, "a1", "us", None, true, "audible")
            .await
            .unwrap();
        for product in products {
            store
                .record_listing(&listing(product), &held_by("a1"))
                .await
                .unwrap();
        }
        store
    }

    /// `(product id, holder)` of every holding, sorted.
    async fn held(store: &LibraryStore) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for h in holdings::Entity::find().all(&store.db).await.unwrap() {
            let listing = store_listings::Entity::find_by_id(h.listing_id)
                .one(&store.db)
                .await
                .unwrap()
                .unwrap();
            out.push((listing.product_id, h.holder));
        }
        out.sort();
        out
    }

    /// `(event type, holder)` of every holding event, in publish order.
    async fn holding_events(store: &LibraryStore) -> Vec<(String, String)> {
        domain_events::Entity::find()
            .filter(domain_events::Column::EventType.is_in([HOLDING_ADDED, HOLDING_REMOVED]))
            .order_by_asc(domain_events::Column::CreatedAt)
            .order_by_asc(domain_events::Column::EventType)
            .all(&store.db)
            .await
            .unwrap()
            .into_iter()
            .map(|e| {
                let payload: serde_json::Value = serde_json::from_str(&e.payload).unwrap();
                (
                    e.event_type,
                    payload["holder"].as_str().unwrap_or_default().to_string(),
                )
            })
            .collect()
    }

    /// Count of events of `event_type`.
    async fn count(store: &LibraryStore, event_type: &str) -> usize {
        domain_events::Entity::find()
            .filter(domain_events::Column::EventType.eq(event_type))
            .all(&store.db)
            .await
            .unwrap()
            .len()
    }

    fn ids(products: &[&str]) -> Vec<String> {
        products.iter().map(|p| (*p).to_string()).collect()
    }

    /// A holding names its account in typed columns, and is never written
    /// for an account the library does not have.
    #[tokio::test]
    async fn a_holding_carries_its_typed_holder_and_needs_its_account() {
        for profile in Profile::ALL {
            let store = library(profile, &["B1"]).await;
            let row = holdings::Entity::find()
                .one(&store.db)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(row.holder, "account:a1");
            assert_eq!(row.holder_kind, "account");
            assert_eq!(row.plugin_key.as_deref(), Some(PLUGIN));
            assert_eq!(row.account_id.as_deref(), Some("a1"));
            assert_eq!(row.node_id, None);

            store
                .record_listing(&listing("B2"), &held_by("ghost"))
                .await
                .unwrap();
            assert_eq!(held(&store).await.len(), 1, "{profile:?}: no ghost holding");
            assert_eq!(count(&store, HOLDING_ADDED).await, 1, "{profile:?}");
        }
    }

    /// Deleting an account row drops its holdings through the foreign key.
    #[tokio::test]
    async fn deleting_an_account_cascades_to_its_holdings() {
        let store = library(Profile::FirstParty, &["B1", "B2"]).await;
        accounts::Entity::delete_many()
            .filter(accounts::Column::AccountId.eq("a1"))
            .exec(&store.db)
            .await
            .unwrap();
        assert!(held(&store).await.is_empty());
    }

    /// A branch holding names its node in typed columns, is written only
    /// while the node exists, and goes with it.
    #[tokio::test]
    async fn a_node_holding_needs_its_node() {
        use sea_orm::ConnectionTrait;
        for profile in Profile::ALL {
            let store = library(profile, &[]).await;
            let by_node = resolve::HoldingDraft {
                holder: resolve::Holder::Node("n1".into()),
                entitlement: resolve::Entitlement::Purchase,
                purchased_at: None,
            };
            store
                .record_listing(&listing("B1"), &by_node)
                .await
                .unwrap();
            assert!(held(&store).await.is_empty(), "{profile:?}: no node yet");

            store
                .db
                .execute_unprepared(
                    "INSERT INTO nodes (node_id, cluster_id, created_at, incarnation, \
                     heartbeat_at, software_version, schema_state, compatible) VALUES \
                     ('n1', 'c1', '2026-01-01T00:00:00Z', 'i', '2026-01-01T00:00:00Z', \
                     'v', 's', 1)",
                )
                .await
                .unwrap();
            store
                .record_listing(&listing("B1"), &by_node)
                .await
                .unwrap();
            assert!(
                held(&store).await.is_empty(),
                "{profile:?}: a node that does not share holds nothing"
            );
            store
                .db
                .execute_unprepared("UPDATE nodes SET share_books = 1 WHERE node_id = 'n1'")
                .await
                .unwrap();
            store
                .db
                .execute_unprepared(
                    "INSERT INTO node_credentials (node_id, sign_public_key, box_public_key, \
                     link_id, enrolled_at, revoked_at) VALUES ('n1', x'00', x'00', 'l1', \
                     '2026-01-01T00:00:00Z', '2026-01-02T00:00:00Z')",
                )
                .await
                .unwrap();
            store
                .record_listing(&listing("B1"), &by_node)
                .await
                .unwrap();
            assert!(
                held(&store).await.is_empty(),
                "{profile:?}: a revoked node holds nothing"
            );
            store
                .db
                .execute_unprepared(
                    "UPDATE node_credentials SET revoked_at = NULL WHERE node_id = 'n1'",
                )
                .await
                .unwrap();
            store
                .record_listing(&listing("B1"), &by_node)
                .await
                .unwrap();
            let row = holdings::Entity::find()
                .one(&store.db)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(row.holder, "node:n1", "{profile:?}");
            assert_eq!(row.holder_kind, "node");
            assert_eq!(row.node_id.as_deref(), Some("n1"));
            assert_eq!(row.plugin_key, None);
            assert_eq!(row.account_id, None);

            // A pass that is no longer the node's removes nothing.
            assert_eq!(
                store
                    .remove_node_holdings(
                        "n1",
                        NodeHoldingSelection::UnseenBy {
                            pass: "p1",
                            fence: 0
                        }
                    )
                    .await
                    .unwrap(),
                1
            );
            assert_eq!(count(&store, HOLDING_REMOVED).await, 0, "{profile:?}");
            store
                .db
                .execute_unprepared(
                    "UPDATE nodes SET catalog_pass_started_at = 'p1' WHERE node_id = 'n1'",
                )
                .await
                .unwrap();
            // Under a catalog fence that is no longer the node's, nothing.
            store
                .remove_node_holdings(
                    "n1",
                    NodeHoldingSelection::UnseenBy {
                        pass: "p1",
                        fence: 1,
                    },
                )
                .await
                .unwrap();
            assert_eq!(count(&store, HOLDING_REMOVED).await, 0, "{profile:?}");
            // A node holding goes with its own event naming no account.
            assert_eq!(
                store
                    .remove_node_holdings(
                        "n1",
                        NodeHoldingSelection::UnseenBy {
                            pass: "p1",
                            fence: 0
                        }
                    )
                    .await
                    .unwrap(),
                1
            );
            assert_eq!(count(&store, HOLDING_REMOVED).await, 1, "{profile:?}");
            store
                .record_listing(&listing("B1"), &by_node)
                .await
                .unwrap();
            store
                .db
                .execute_unprepared("UPDATE holdings SET seen_pass = 'p2'")
                .await
                .unwrap();
            store
                .db
                .execute_unprepared(
                    "UPDATE nodes SET catalog_pass_started_at = 'p2' WHERE node_id = 'n1'",
                )
                .await
                .unwrap();
            assert_eq!(
                store
                    .remove_node_holdings(
                        "n1",
                        NodeHoldingSelection::UnseenBy {
                            pass: "p2",
                            fence: 0
                        }
                    )
                    .await
                    .unwrap(),
                0,
                "{profile:?}: a holding the pass named stays"
            );

            store
                .db
                .execute_unprepared("DELETE FROM nodes WHERE node_id = 'n1'")
                .await
                .unwrap();
            assert!(held(&store).await.is_empty(), "{profile:?}: cascades");
        }
    }

    /// A delta page's removed ids drop exactly those holdings, each with
    /// one `holding_removed`; a second removal publishes nothing.
    #[tokio::test]
    async fn removed_products_lose_their_holdings_once() {
        for profile in Profile::ALL {
            let store = library(profile, &["B1", "B2", "B3"]).await;
            // A guest's padded spelling meets the stored listing.
            let gone = ids(&[" B2 ", "B3", "B3", "never-held"]);
            let n = store
                .remove_account_holdings(PLUGIN, "a1", HoldingSelection::Products(&gone))
                .await
                .unwrap();
            assert_eq!(n, 2, "{profile:?}");
            assert_eq!(
                held(&store).await,
                [("B1".to_string(), "account:a1".to_string())]
            );
            assert_eq!(count(&store, HOLDING_REMOVED).await, 2, "{profile:?}");
            store
                .remove_account_holdings(PLUGIN, "a1", HoldingSelection::Products(&gone))
                .await
                .unwrap();
            assert_eq!(count(&store, HOLDING_REMOVED).await, 2, "{profile:?}");
        }
    }

    /// A complete full pass removes the holdings it did not stamp, and
    /// leaves those it did, including one stamped by a later pass.
    #[tokio::test]
    async fn a_full_pass_removes_what_it_did_not_list() {
        for profile in Profile::ALL {
            let store = library(profile, &["B1", "B2", "B3", "B4"]).await;
            store
                .mark_holdings_seen(PLUGIN, "a1", "pass-1", &ids(&[" B1", "B3 "]))
                .await
                .unwrap();
            // A concurrent pass listed B4 after this pass read it.
            store
                .mark_holdings_seen(PLUGIN, "a1", "pass-2", &ids(&["B4"]))
                .await
                .unwrap();
            let n = store
                .remove_account_holdings(PLUGIN, "a1", HoldingSelection::UnseenBy("pass-1"))
                .await
                .unwrap();
            assert_eq!(n, 2, "{profile:?}: B2 and B4 read as unseen");
            assert_eq!(
                held(&store)
                    .await
                    .into_iter()
                    .map(|(p, _)| p)
                    .collect::<Vec<_>>(),
                ["B1", "B3"],
                "{profile:?}"
            );
            assert_eq!(count(&store, HOLDING_REMOVED).await, 2, "{profile:?}");
        }
    }

    /// The guard re-checks the pass: a holding stamped between the read
    /// and the delete stays, and publishes nothing.
    #[tokio::test]
    async fn a_holding_stamped_after_the_read_is_kept() {
        let store = library(Profile::FirstParty, &["B1"]).await;
        let row = holdings::Entity::find()
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        store
            .mark_holdings_seen(PLUGIN, "a1", "pass-1", &ids(&["B1"]))
            .await
            .unwrap();
        let doomed = [Doomed {
            id: row.id,
            listing_id: row.listing_id,
            holder: row.holder,
            account_id: "a1".into(),
            source: PLUGIN.into(),
        }];
        store
            .delete_holdings(
                Scope::Account {
                    plugin_key: PLUGIN,
                    account_id: "a1",
                },
                &doomed,
                DeleteGuard::UnseenBy("pass-1"),
            )
            .await
            .unwrap();
        assert_eq!(held(&store).await.len(), 1);
        assert_eq!(count(&store, HOLDING_REMOVED).await, 0);
    }

    /// Many holdings are removed in keyset pages and size-limited batches.
    #[tokio::test]
    async fn a_large_removal_pages_through_every_holding() {
        let products: Vec<String> = (0..30).map(|i| format!("P{i:02}")).collect();
        let refs: Vec<&str> = products.iter().map(String::as_str).collect();
        for profile in Profile::ALL {
            let store = library(profile, &refs).await;
            let n = store
                .remove_account_holdings(PLUGIN, "a1", HoldingSelection::All)
                .await
                .unwrap();
            assert_eq!(n, 30, "{profile:?}");
            assert!(held(&store).await.is_empty(), "{profile:?}");
            assert_eq!(count(&store, HOLDING_REMOVED).await, 30, "{profile:?}");
        }
    }

    /// Stamping and removing by long product ids (UUIDs) sends only
    /// statements the backend admits: on D1 a bind-count-wide chunk of
    /// UUIDs is past `maxPayloadBytes`, so the chunks narrow.
    #[tokio::test]
    async fn long_product_ids_fit_the_payload_cap_under_every_profile() {
        for profile in Profile::ALL {
            let store = library(profile, &["B1", "B2", "B3"]).await;
            let mut listed: Vec<String> =
                (0..300).map(|_| uuid::Uuid::new_v4().to_string()).collect();
            listed.push("B1".into());
            listed.push("B3".into());
            store
                .mark_holdings_seen(PLUGIN, "a1", "pass-1", &listed)
                .await
                .unwrap();
            store
                .remove_account_holdings(PLUGIN, "a1", HoldingSelection::UnseenBy("pass-1"))
                .await
                .unwrap();
            assert_eq!(
                held(&store).await,
                [
                    ("B1".to_string(), "account:a1".to_string()),
                    ("B3".to_string(), "account:a1".to_string())
                ],
                "{profile:?}: B2 went, the stamped ones stayed"
            );

            let mut gone: Vec<String> =
                (0..300).map(|_| uuid::Uuid::new_v4().to_string()).collect();
            gone.push("B3".into());
            let n = store
                .remove_account_holdings(PLUGIN, "a1", HoldingSelection::Products(&gone))
                .await
                .unwrap();
            assert_eq!(n, 1, "{profile:?}");
            assert_eq!(held(&store).await.len(), 1, "{profile:?}");
        }
    }

    /// A title removed and held again publishes `holding_added` again.
    #[tokio::test]
    async fn a_holding_held_again_announces_itself_again() {
        let store = library(Profile::D1, &["B1"]).await;
        store
            .remove_account_holdings(PLUGIN, "a1", HoldingSelection::All)
            .await
            .unwrap();
        store
            .record_listing(&listing("B1"), &held_by("a1"))
            .await
            .unwrap();
        assert_eq!(
            holding_events(&store).await,
            [
                (HOLDING_ADDED.to_string(), "account:a1".to_string()),
                (HOLDING_REMOVED.to_string(), "account:a1".to_string()),
                (HOLDING_ADDED.to_string(), "account:a1".to_string()),
            ]
        );
    }

    /// Revoking an account removes its holdings and its sync token, so the
    /// next pass is full and adds them back.
    #[tokio::test]
    async fn revoking_an_account_removes_its_holdings_and_token() {
        for profile in Profile::ALL {
            let store = library(profile, &["B1", "B2"]).await;
            store
                .finish_scan_pass(PLUGIN, "a1", "tok", true)
                .await
                .unwrap();
            store.revoke_credentials(PLUGIN, "a1").await.unwrap();
            assert!(held(&store).await.is_empty(), "{profile:?}");
            assert_eq!(count(&store, HOLDING_REMOVED).await, 2, "{profile:?}");
            let state = store.scan_state(PLUGIN, "a1").await.unwrap().unwrap();
            assert_eq!(state.sync_token, "", "{profile:?}");

            // A page still in flight when the account was revoked adds
            // nothing back; reconnecting lets the next scan hold it again.
            store
                .record_listing(&listing("B1"), &held_by("a1"))
                .await
                .unwrap();
            assert!(held(&store).await.is_empty(), "{profile:?}: revoked");
            store.mark_connection_active(PLUGIN, "a1").await.unwrap();
            store
                .record_listing(&listing("B1"), &held_by("a1"))
                .await
                .unwrap();
            assert_eq!(held(&store).await.len(), 1, "{profile:?}: reconnected");
        }
    }

    /// Metadata from elsewhere updates a title without holding it again.
    #[tokio::test]
    async fn a_metadata_update_does_not_hold_a_removed_title() {
        for profile in Profile::ALL {
            let store = library(profile, &["B1"]).await;
            store
                .remove_account_holdings(PLUGIN, "a1", HoldingSelection::All)
                .await
                .unwrap();
            let mut book = crate::NewBook::minimal("B1", "a1", "us", "Title B1");
            book.plugin_key = PLUGIN.into();
            store.update_book_metadata(&book).await.unwrap();
            assert!(held(&store).await.is_empty(), "{profile:?}");
            store.upsert_book(&book).await.unwrap();
            assert_eq!(held(&store).await.len(), 1, "{profile:?}: upsert holds it");
        }
    }

    /// A remap moves holdings to the new id with an event pair each, and a
    /// holding the new id already has is kept rather than duplicated.
    #[tokio::test]
    async fn remapping_an_account_moves_its_holdings() {
        for profile in Profile::ALL {
            let store = library(profile, &["B1", "B2"]).await;
            store
                .upsert_account(PLUGIN, "a2", "us", None, true, "audible")
                .await
                .unwrap();
            store
                .record_listing(&listing("B2"), &held_by("a2"))
                .await
                .unwrap();
            store.remap_account_id(PLUGIN, "a1", "a2").await.unwrap();
            assert_eq!(
                held(&store).await,
                [
                    ("B1".to_string(), "account:a2".to_string()),
                    ("B2".to_string(), "account:a2".to_string()),
                ],
                "{profile:?}"
            );
            let moved = holdings::Entity::find()
                .filter(holdings::Column::AccountId.eq("a2"))
                .all(&store.db)
                .await
                .unwrap();
            assert!(moved.iter().all(|h| h.account_id.as_deref() == Some("a2")));
            // B1 and B2 each leave a1; only B1 is new for a2.
            assert_eq!(count(&store, HOLDING_REMOVED).await, 2, "{profile:?}");
            assert_eq!(
                count(&store, HOLDING_ADDED).await,
                4,
                "{profile:?}: a1's two, a2's B2, and B1 moved"
            );
        }
    }

    /// Stamping reaches only the account's own holdings.
    #[tokio::test]
    async fn stamping_is_scoped_to_the_account() {
        let store = library(Profile::FirstParty, &["B1"]).await;
        store
            .upsert_account(PLUGIN, "a2", "us", None, true, "audible")
            .await
            .unwrap();
        store
            .record_listing(&listing("B1"), &held_by("a2"))
            .await
            .unwrap();
        store
            .mark_holdings_seen(PLUGIN, "a1", "pass-1", &ids(&["B1"]))
            .await
            .unwrap();
        let rows = holdings::Entity::find()
            .order_by_asc(holdings::Column::Holder)
            .all(&store.db)
            .await
            .unwrap();
        let seen: Vec<_> = rows.iter().map(|h| h.seen_pass.as_deref()).collect();
        assert_eq!(seen, [Some("pass-1"), None]);
    }
}
