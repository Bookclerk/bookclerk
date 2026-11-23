//! Copy a library between database backends (sqlite ↔ d1 ↔ postgres).
//!
//! Used when an operator switches `[database].plugin` and wants to move existing
//! library state to the new backend. This is always opt-in — config reload and
//! plugin enablement do not run it automatically.
//!
//! The copy carries the whole library state, not only the catalog: the
//! library identity, every configuration document with its revision, audit
//! and change rows, plugin versions, artifacts, instances, deployments and
//! placements, nodes and their credentials, links to other libraries, queued
//! jobs and events, and the atomic-operation receipts. The destination then
//! opens as the same library at the same configuration revisions, so nothing
//! is re-seeded from TOML.
//!
//! [`LEFT_BEHIND_TABLES`] lists the tables the copy skips on purpose, each
//! with the reason: derived data the library rebuilds, liveness rows every
//! node rewrites, and plugin database units that belong to the old adapter.
//! A test fails when a new table is neither copied nor listed there.

use sea_orm::{
    ConnectionTrait, DatabaseConnection, EntityName, EntityTrait, FromQueryResult, PaginatorTrait,
    Statement, TransactionTrait,
};

use bookclerk_db_exec::canonical_statement;

use crate::column_read::only_column;
use crate::control_plane::{load_artifact_blob, put_artifact_blob};
use crate::entities::{
    account_credential_revisions, account_downloads, account_links, account_pace,
    account_scan_state, account_shares, accounts, admin_controls, bookclerk_receipts, bookmarks,
    catalog_listing_optouts, catalog_lookups, chapter_list_entries, chapter_lists, claim_tickets,
    clerk_links, configuration_audit, configuration_changes, configuration_documents, contributors,
    copy_grants, credits, domain_events, edition_acquisitions, edition_chapters, edition_copies,
    edition_files, edition_identifiers, editions, embeddings, encrypted_secrets, event_deliveries,
    event_outbox_stats, expressions, holdings, ignored_titles, ingest_batches, ingest_members,
    intake_items, job_secret_handoffs, job_temp_paths, jobs, library_identity, library_links,
    link_copy_files, link_imports, link_requests, listening_progress, listing_offers,
    metadata_claims, node_credentials, node_enrollment_links, node_upload_sessions, nodes,
    notifications, notify_deliveries, notify_destinations, notify_inbox, notify_inbox_routes,
    notify_patron_offers, notify_patron_prefs, notify_patron_targets, notify_route_revisions,
    notify_routes, oidc_auth_codes, oidc_clients, oidc_refresh_tokens, oidc_rp_states,
    operator_sessions, patron_controls, patron_scope_keys, patron_scopes, pending_joins,
    playback_progress, playback_sessions, player_card_tokens, player_cards, player_passwords,
    plugin_artifacts, plugin_cron_cursors, plugin_deployments, plugin_installs, plugin_instances,
    plugin_kv, plugin_placements, plugin_schemas, plugin_versions, plugin_webhooks,
    portal_identities, portal_sessions, remote_plugin_consents, resolution_conflicts,
    resolution_overrides, saved_filters, security_audit_events, series, series_entries,
    store_listings, title_request_sources, title_requests, user_invites, user_preferences,
    user_title_state, user_work_tags, users, webauthn_challenges, webauthn_credentials,
    window_book_parts, window_books, work_identifiers, work_tags, works,
};
use crate::error::{LibraryError, Result};
use crate::store::LibraryStore;

/// Tables the copy leaves behind on purpose, each with the reason.
///
/// Every other table of the library schema is copied; a test checks that
/// the two lists together cover the schema.
pub const LEFT_BEHIND_TABLES: &[(&str, &str)] = &[
    ("discover_candidates", "derived: Discover jobs refetch it"),
    (
        "plugin_webhook_chunks",
        "transient: bodies of stored webhook requests, which the plugins' listing fallback covers",
    ),
    (
        "plugin_webhook_deliveries",
        "transient: stored webhook requests, which the plugins' listing fallback covers",
    ),
    ("discover_feeds", "derived: Discover jobs re-rank it"),
    ("discover_fetches", "derived: Discover jobs refetch it"),
    (
        "discover_request_queue",
        "derived: the ranking job recomputes it",
    ),
    (
        "discover_store_call_hits",
        "derived: a storefront answer cache",
    ),
    ("discover_store_calls", "derived: a storefront answer cache"),
    ("discover_tastes", "derived: rebuilt from per-user state"),
    (
        "library_index_builds",
        "derived: with no build row the heartbeat rebuilds the page index",
    ),
    ("library_work_rows", "derived: the page index, rebuilt"),
    ("library_work_facets", "derived: the page index, rebuilt"),
    (
        "storage_scan_generations",
        "an in-progress storage scan, which starts over",
    ),
    (
        "storage_scan_rows",
        "an in-progress storage scan, which starts over",
    ),
    (
        "destination_objects",
        "derived: each destination's next listing rebuilds it",
    ),
    (
        "destination_feeds",
        "derived: feed state and listing schedule, started over",
    ),
    ("library_leases", "maintenance leases, taken again"),
    (
        "node_status",
        "each daemon rewrites its heartbeat every 15 s",
    ),
    (
        "event_subscriber_nodes",
        "live subscriber registrations, re-sent on each heartbeat",
    ),
    (
        "plugin_deployment_observations",
        "observed state, reported again by each node",
    ),
    (
        "live_topic_revisions",
        "page reload counters, seeded by the schema",
    ),
    (
        "plugin_databases",
        "plugin database units belong to the old adapter; the new one provisions its own",
    ),
    (
        "plugin_artifact_chunks",
        "rewritten with plugin_artifacts at the destination's chunk size",
    ),
    (
        "bookclerk_slots",
        "the destination's own migration slots and short-lived compare-and-swap guards; \
         a replayed operation is answered from bookclerk_receipts",
    ),
    (
        "bookclerk_schema_migrations",
        "the destination's own schema bookkeeping",
    ),
    (
        "bookclerk_sql_catalog",
        "the destination's own schema bookkeeping",
    ),
    (
        "bookclerk_sql_ddl",
        "the destination's own schema bookkeeping",
    ),
    (
        "bookclerk_sql_schema",
        "the destination's own schema bookkeeping",
    ),
];

/// Options for a backend-to-backend library copy.
#[derive(Debug, Clone)]
pub struct BackendMigrateOptions {
    /// When true, report counts only — do not write to the destination.
    pub dry_run: bool,
    /// Allow copying into a destination that already has library rows.
    pub force: bool,
}

/// Per-table row counts copied (or that would be copied in dry-run mode).
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct BackendMigrateSummary {
    /// Map of table name → rows copied (or counted in dry-run).
    /// `plugin_artifacts` counts archives; their chunks are not counted.
    pub tables: std::collections::BTreeMap<String, usize>,
    /// When true, counts rows without writing to the destination.
    pub dry_run: bool,
}

impl BackendMigrateSummary {
    /// Sums row counts across all tables in the summary, leaving out
    /// `plugin_artifacts`, which counts archives rather than rows.
    #[must_use]
    pub fn total_rows(&self) -> usize {
        self.tables
            .iter()
            .filter(|(table, _)| table.as_str() != "plugin_artifacts")
            .map(|(_, n)| *n)
            .sum()
    }
}

/// Calls `$each!(entity_module)` for every table copied row by row, in an
/// order that inserts each row after the rows its foreign keys name.
macro_rules! copied_entities {
    ($each:ident) => {
        $each!(library_identity);
        $each!(configuration_documents);
        $each!(configuration_changes);
        $each!(configuration_audit);
        $each!(users);
        $each!(nodes);
        $each!(node_credentials);
        $each!(node_enrollment_links);
        $each!(accounts);
        $each!(account_scan_state);
        // Pace overrides and cool-downs, and the download ledger, so the
        // rolling daily budget still counts downloads made before the move.
        $each!(account_pace);
        $each!(account_downloads);
        $each!(portal_identities);
        // Shows before their episodes (`works.parent_work_id`).
        $each!(works);
        $each!(work_identifiers);
        $each!(work_tags);
        $each!(expressions);
        $each!(editions);
        $each!(edition_identifiers);
        $each!(contributors);
        $each!(credits);
        $each!(series);
        $each!(series_entries);
        $each!(store_listings);
        $each!(listing_offers);
        $each!(holdings);
        $each!(resolution_overrides);
        $each!(resolution_conflicts);
        $each!(edition_files);
        $each!(edition_acquisitions);
        $each!(edition_copies);
        // A list before the claims and the resolution that name it.
        $each!(chapter_lists);
        $each!(chapter_list_entries);
        $each!(metadata_claims);
        $each!(edition_chapters);
        $each!(ignored_titles);
        $each!(saved_filters);
        $each!(claim_tickets);
        $each!(user_invites);
        $each!(portal_sessions);
        $each!(player_passwords);
        $each!(player_cards);
        $each!(player_card_tokens);
        $each!(operator_sessions);
        $each!(account_links);
        $each!(account_shares);
        $each!(account_credential_revisions);
        $each!(listening_progress);
        $each!(title_requests);
        $each!(title_request_sources);
        $each!(user_title_state);
        $each!(user_work_tags);
        $each!(playback_progress);
        $each!(playback_sessions);
        $each!(bookmarks);
        // Notification setup, the inbox and the delivery log; the log's
        // send jobs move with `jobs` below.
        $each!(notify_destinations);
        $each!(notify_routes);
        $each!(notify_inbox_routes);
        $each!(notify_route_revisions);
        $each!(notify_patron_offers);
        $each!(notify_patron_prefs);
        $each!(notify_patron_targets);
        $each!(notifications);
        $each!(notify_deliveries);
        $each!(notify_inbox);
        $each!(embeddings);
        $each!(user_preferences);
        $each!(patron_controls);
        $each!(admin_controls);
        $each!(patron_scopes);
        $each!(patron_scope_keys);
        $each!(encrypted_secrets);
        $each!(security_audit_events);
        $each!(oidc_clients);
        $each!(oidc_auth_codes);
        $each!(oidc_refresh_tokens);
        $each!(oidc_rp_states);
        $each!(webauthn_credentials);
        $each!(webauthn_challenges);
        $each!(plugin_schemas);
        $each!(plugin_versions);
        $each!(plugin_installs);
        $each!(plugin_instances);
        // Drop folder items, so `leave` folders are not added again.
        $each!(intake_items);
        $each!(plugin_deployments);
        $each!(plugin_placements);
        $each!(plugin_cron_cursors);
        $each!(plugin_kv);
        $each!(plugin_webhooks);
        $each!(remote_plugin_consents);
        $each!(library_links);
        $each!(clerk_links);
        $each!(pending_joins);
        $each!(link_copy_files);
        $each!(link_imports);
        $each!(link_requests);
        $each!(catalog_listing_optouts);
        $each!(catalog_lookups);
        $each!(copy_grants);
        $each!(window_books);
        $each!(window_book_parts);
        $each!(node_upload_sessions);
        $each!(jobs);
        // Uploads keep their outcomes and review list across the move.
        $each!(ingest_batches);
        $each!(ingest_members);
        $each!(job_temp_paths);
        $each!(job_secret_handoffs);
        $each!(domain_events);
        $each!(event_deliveries);
        $each!(event_outbox_stats);
        $each!(bookclerk_receipts);
    };
}

/// Tables with no SeaORM entity, copied with canonical SQL after the
/// entity tables: `(table, row shape)`. Every column is read and written
/// by name.
const RAW_TABLES: &[(&str, RawTable)] = &[("plugin_instance_defaults", RawTable::InstanceDefaults)];

/// The row shape of one [`RAW_TABLES`] entry.
#[derive(Debug, Clone, Copy)]
enum RawTable {
    /// `plugin_instance_defaults (plugin_key, plugin_instance_id)`.
    InstanceDefaults,
}

/// One `plugin_instance_defaults` row: the default instance claim of a plugin key.
#[derive(Debug, FromQueryResult)]
struct InstanceDefaultRow {
    /// Canonical plugin key the claim is for.
    plugin_key: String,
    /// The instance hosts route to when a job pins none.
    plugin_instance_id: String,
}

/// Copy the whole library from `source` into `dest`.
///
/// The destination should be empty unless [`BackendMigrateOptions::force`] is set.
/// Plugin artifacts are written first through the destination's own chunked
/// writer (they are content-addressed, so a second run rewrites nothing);
/// every other row is then copied in a single transaction on the
/// destination.
///
/// # Arguments
///
/// * `source` - Existing library backend to read from.
/// * `dest` - Target backend to write into (must already be migrated).
/// * `opts` - Dry-run / force behavior.
///
/// # Returns
///
/// Per-table row counts copied (or that would be copied when `dry_run`).
///
/// # Errors
///
/// Returns [`LibraryError`] when the destination is non-empty without `force`,
/// or when any read/write fails.
pub async fn migrate_library_backend(
    source: &LibraryStore,
    dest: &LibraryStore,
    opts: &BackendMigrateOptions,
) -> Result<BackendMigrateSummary> {
    if !opts.force && !opts.dry_run {
        let existing = dest_row_count(dest.db()).await?;
        if existing > 0 {
            return Err(LibraryError::Other(anyhow::anyhow!(
                "destination already has {existing} library row(s); pass force to overwrite \
                 (existing rows are not deleted automatically)"
            )));
        }
    }

    let mut summary = BackendMigrateSummary {
        dry_run: opts.dry_run,
        ..Default::default()
    };

    if opts.dry_run {
        summary.tables = dry_run_counts(source.db()).await?;
        return Ok(summary);
    }

    // An operator tool, run once, that needs every row in one transaction:
    // the first-party sqlite and Postgres adapters accept the host-private
    // interactive transaction (D1's does not; see
    // docs/plugins/database-limits.md). Open one and read inside it before
    // writing anything, so a destination that refuses it is left untouched.
    #[allow(clippy::disallowed_methods)]
    let probe = dest.db().begin().await.map_err(LibraryError::Orm)?;
    // An adapter that refuses answers the first statement inside it.
    plugin_artifacts::Entity::find()
        .count(&probe)
        .await
        .map_err(LibraryError::Orm)?;
    probe.rollback().await.map_err(LibraryError::Orm)?;

    // Archives go first, outside the row transaction: the chunked writer
    // runs its own atomic batches sized for the destination's limits.
    summary.tables.insert(
        plugin_artifacts::Entity.table_name().into(),
        copy_artifacts(source, dest).await?,
    );

    let source_db = source.db();
    // The transaction the probe above showed the destination accepts.
    #[allow(clippy::disallowed_methods)]
    let txn = dest.db().begin().await.map_err(LibraryError::Orm)?;

    clear_replaced_rows(&txn).await?;

    macro_rules! copy {
        (works) => {{
            let n = copy_works(source_db, &txn).await?;
            summary.tables.insert(works::Entity.table_name().into(), n);
        }};
        ($entity:ident) => {{
            let n = copy_entity::<$entity::Entity>(source_db, &txn).await?;
            summary
                .tables
                .insert($entity::Entity.table_name().into(), n);
        }};
    }
    copied_entities!(copy);
    for (table, shape) in RAW_TABLES {
        let n = copy_raw(source_db, &txn, *shape).await?;
        summary.tables.insert((*table).into(), n);
    }

    txn.commit().await.map_err(LibraryError::Orm)?;
    Ok(summary)
}

/// Copies every stored plugin archive through the destination's chunked
/// writer, so the chunks fit the destination's advertised limits.
async fn copy_artifacts(source: &LibraryStore, dest: &LibraryStore) -> Result<usize> {
    let headers = plugin_artifacts::Entity::find()
        .all(source.db())
        .await
        .map_err(LibraryError::Orm)?;
    let mut copied = 0usize;
    for header in headers {
        let bytes = load_artifact_blob(source, &header.sha256)
            .await?
            .ok_or_else(|| {
                LibraryError::Other(anyhow::anyhow!(
                    "plugin artifact {} has a header but no stored bytes",
                    header.sha256
                ))
            })?;
        let written = put_artifact_blob(dest, &bytes).await?;
        if written.strip_prefix("blob:sha256:") != Some(header.sha256.as_str()) {
            return Err(LibraryError::Other(anyhow::anyhow!(
                "plugin artifact {} read back as {written}",
                header.sha256
            )));
        }
        copied += 1;
    }
    Ok(copied)
}

/// Copies every row of one SeaORM entity from `source` into the destination transaction.
async fn copy_entity<E>(source: &DatabaseConnection, dest: &impl ConnectionTrait) -> Result<usize>
where
    E: EntityTrait,
    E::Model: Into<<E as EntityTrait>::ActiveModel>,
{
    let models = E::find().all(source).await.map_err(LibraryError::Orm)?;
    let count = models.len();
    for model in models {
        let active: E::ActiveModel = model.into();
        E::insert(active)
            .exec(dest)
            .await
            .map_err(LibraryError::Orm)?;
    }
    Ok(count)
}

/// Copies `works` parents first, so an engine that checks the
/// self-referencing `parent_work_id` key on every insert accepts each row.
///
/// Only shows are parents and they have no parent of their own, so one
/// pass that puts parentless works first is enough.
async fn copy_works(source: &DatabaseConnection, dest: &impl ConnectionTrait) -> Result<usize> {
    let mut models = works::Entity::find()
        .all(source)
        .await
        .map_err(LibraryError::Orm)?;
    models.sort_by_key(|m| m.parent_work_id.is_some());
    let count = models.len();
    for model in models {
        let active: works::ActiveModel = model.into();
        works::Entity::insert(active)
            .exec(dest)
            .await
            .map_err(LibraryError::Orm)?;
    }
    Ok(count)
}

/// A canonical (SQL-v1) statement with no parameters.
fn canonical(sql: &str) -> Statement {
    canonical_statement(sql, [])
}

/// Copies one table that has no SeaORM entity, reading and writing every
/// column by name with canonical SQL.
async fn copy_raw(
    source: &DatabaseConnection,
    dest: &impl ConnectionTrait,
    shape: RawTable,
) -> Result<usize> {
    let inserts: Vec<Statement> = match shape {
        RawTable::InstanceDefaults => InstanceDefaultRow::find_by_statement(canonical(
            "SELECT plugin_key, plugin_instance_id FROM plugin_instance_defaults",
        ))
        .all(source)
        .await
        .map_err(LibraryError::Orm)?
        .into_iter()
        .map(|row| {
            canonical_statement(
                "INSERT INTO plugin_instance_defaults (plugin_key, plugin_instance_id) \
                 VALUES (?, ?)",
                [row.plugin_key.into(), row.plugin_instance_id.into()],
            )
        })
        .collect(),
    };
    let count = inserts.len();
    for insert in inserts {
        dest.execute_raw(insert).await.map_err(LibraryError::Orm)?;
    }
    Ok(count)
}

/// Deletes the rows a destination gets before any library data: the
/// schema's `event_outbox_stats` singleton, and the identity, node row and
/// seeded configuration documents a host writes when it first opens the
/// database. The source's rows replace them.
///
/// Deleted children first, in the reverse of their copy order.
async fn clear_replaced_rows(dest: &impl ConnectionTrait) -> Result<()> {
    macro_rules! clear {
        ($entity:ident) => {{
            $entity::Entity::delete_many()
                .exec(dest)
                .await
                .map_err(LibraryError::Orm)?;
        }};
    }
    clear!(event_outbox_stats);
    clear!(nodes);
    clear!(configuration_audit);
    clear!(configuration_changes);
    clear!(configuration_documents);
    clear!(library_identity);
    Ok(())
}

/// Tables whose destination rows [`clear_replaced_rows`] replaces, so they
/// do not make a destination count as non-empty.
const REPLACED_TABLES: [&str; 6] = [
    "event_outbox_stats",
    "nodes",
    "configuration_audit",
    "configuration_changes",
    "configuration_documents",
    "library_identity",
];

/// Sums the copied tables' row counts on the destination (used to refuse a
/// non-empty target).
///
/// Plugin archives are not counted: they are content-addressed and the
/// writer skips an archive that is already stored, so a run that stopped
/// after writing them can be repeated. Nor are [`REPLACED_TABLES`]: a host
/// that opened the destination before the copy seeded them, and the copy
/// replaces them.
async fn dest_row_count(db: &DatabaseConnection) -> Result<usize> {
    let counts = dry_run_counts(db).await?;
    Ok(counts
        .iter()
        .filter(|(table, _)| {
            table.as_str() != plugin_artifacts::Entity.table_name()
                && !REPLACED_TABLES.contains(&table.as_str())
        })
        .map(|(_, n)| *n)
        .sum())
}

/// Row count as a `usize`, saturating on a target narrower than 64 bits.
fn as_count(n: u64) -> usize {
    usize::try_from(n).unwrap_or(usize::MAX)
}

/// Counts rows per copied table without writing (dry-run summary).
async fn dry_run_counts(
    source: &DatabaseConnection,
) -> Result<std::collections::BTreeMap<String, usize>> {
    let mut tables = std::collections::BTreeMap::new();
    macro_rules! count {
        ($entity:ident) => {{
            let n = $entity::Entity::find()
                .count(source)
                .await
                .map_err(LibraryError::Orm)?;
            tables.insert($entity::Entity.table_name().into(), as_count(n));
        }};
    }
    count!(plugin_artifacts);
    copied_entities!(count);
    for (table, _) in RAW_TABLES {
        let row = source
            .query_one_raw(canonical(&format!("SELECT COUNT(*) AS n FROM {table}")))
            .await
            .map_err(LibraryError::Orm)?;
        let n = match row {
            Some(row) => only_column::<i64>(&row).map_err(LibraryError::Orm)?,
            None => 0,
        };
        tables.insert(
            (*table).into(),
            usize::try_from(n).map_err(|_| {
                LibraryError::Other(anyhow::anyhow!("{table} reported a negative row count"))
            })?,
        );
    }
    Ok(tables)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::control_plane::{
        assigned_patron_scope, audit_count, bootstrap_control_plane, change_count,
        default_instance_for_key, ensure_plugin_instance, load_default_patron_scope,
        replace_events, replace_output, set_default_patron_scope, verify_patron_administrator,
        ConfigActor, EventsReplace, OutputReplace, PatronScope, RuleEffect, ScopeField, ScopeRule,
        EVENTS_NAMESPACE, OUTPUT_NAMESPACE,
    };
    use crate::master_key::master_key_test_lock_async;
    use crate::models::UserRole;
    use bookclerk_config::{Config, NamingProfile, OutputConfig};
    use sea_orm::{ActiveModelTrait, ActiveValue::Set, IntoActiveModel};
    use tempfile::tempdir;

    /// An empty, migrated in-memory library.
    async fn memory_store() -> LibraryStore {
        LibraryStore::from_connection(
            bookclerk_plugin_database_sqlite::open_memory()
                .await
                .unwrap(),
        )
    }

    /// Copy options that write.
    fn apply() -> BackendMigrateOptions {
        BackendMigrateOptions {
            dry_run: false,
            force: false,
        }
    }

    /// A work row with `id`, `kind` and no parent.
    fn work(id: &str, kind: &str) -> works::ActiveModel {
        works::ActiveModel {
            id: Set(id.into()),
            kind: Set(kind.into()),
            title: Set(id.into()),
            match_key: Set(id.into()),
            parent_work_id: Set(None),
            description: Set(None),
            subjects: Set(None),
            categories: Set(None),
            cover_url: Set(None),
            created_at: Set("2026-01-01T00:00:00Z".into()),
            updated_at: Set("2026-01-01T00:00:00Z".into()),
        }
    }

    #[tokio::test]
    async fn the_download_ledger_copies_so_the_pace_window_survives() {
        let source = memory_store().await;
        let dest = memory_store().await;
        accounts::ActiveModel {
            id: Set(1),
            plugin_key: Set("path:file:///tmp/store#store".into()),
            account_id: Set("acct".into()),
            marketplace: Set("us".into()),
            label: Set(None),
            scan_enabled: Set(1),
            source: Set("store".into()),
            connection_status: Set("active".into()),
            created_at: Set("2026-01-01T00:00:00Z".into()),
            updated_at: Set("2026-01-01T00:00:00Z".into()),
        }
        .insert(source.db())
        .await
        .unwrap();
        for (id, paced) in [("d1", 1), ("d2", 0)] {
            account_downloads::ActiveModel {
                id: Set(id.into()),
                plugin_key: Set("path:file:///tmp/store#store".into()),
                account_id: Set("acct".into()),
                started_at_ms: Set(1_767_225_600_000),
                job_id: Set(None),
                paced: Set(paced),
            }
            .insert(source.db())
            .await
            .unwrap();
        }
        let summary = migrate_library_backend(&source, &dest, &apply())
            .await
            .unwrap();
        assert_eq!(summary.tables["account_downloads"], 2);
        let copied = account_downloads::Entity::find()
            .all(dest.db())
            .await
            .unwrap();
        assert_eq!(copied.len(), 2, "the destination keeps the rolling window");
        assert!(copied
            .iter()
            .any(|row| row.id == "d1" && row.paced == 1 && row.account_id == "acct"));
    }

    #[tokio::test]
    async fn episodes_stored_before_their_show_still_copy() {
        let source = memory_store().await;
        let dest = memory_store().await;
        // The episode's row comes first in storage order, then gains its
        // parent once the show exists.
        work("w_episode", "podcast")
            .insert(source.db())
            .await
            .unwrap();
        work("w_show", "podcast").insert(source.db()).await.unwrap();
        let mut episode = works::Entity::find_by_id("w_episode")
            .one(source.db())
            .await
            .unwrap()
            .unwrap()
            .into_active_model();
        episode.parent_work_id = Set(Some("w_show".into()));
        episode.update(source.db()).await.unwrap();

        let summary = migrate_library_backend(&source, &dest, &apply())
            .await
            .unwrap();
        assert_eq!(summary.tables["works"], 2);
        let copied = works::Entity::find_by_id("w_episode")
            .one(dest.db())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(copied.parent_work_id.as_deref(), Some("w_show"));

        let dry = migrate_library_backend(
            &source,
            &dest,
            &BackendMigrateOptions {
                dry_run: true,
                force: false,
            },
        )
        .await
        .unwrap();
        assert!(dry.dry_run);
        assert_eq!(dry.tables["store_listings"], 0);
        assert!(migrate_library_backend(&source, &dest, &apply())
            .await
            .is_err());
    }

    /// Configuration documents keep their bodies and revisions, the library
    /// keeps its identity, and a restrictive default patron scope still
    /// applies to a patron with no scope of their own, so reopening the
    /// destination re-seeds nothing from TOML.
    #[tokio::test]
    async fn configuration_and_cluster_state_survive_the_copy() {
        let _guard = master_key_test_lock_async().await;
        let files = tempdir().unwrap();
        let scratch = files.path().join("cache");
        let source = memory_store().await;
        let dest = memory_store().await;
        let operator = ConfigActor::Operator {
            id: "operator".into(),
        };

        let seed = Config {
            output: OutputConfig {
                widevine: true,
                ..OutputConfig::default()
            },
            ..Config::default()
        };
        let session = bootstrap_control_plane(&source, files.path(), &scratch, None, &seed)
            .await
            .unwrap();

        let mut output_body = session.output.body.clone();
        output_body.naming_profile = NamingProfile::Audiobookshelf;
        let OutputReplace::Applied(output) = replace_output(
            &source,
            &operator,
            session.output.revision,
            &output_body,
            "migrate-test-output",
        )
        .await
        .unwrap() else {
            panic!("output replace did not apply");
        };
        let mut events_body = session.events.body.clone();
        events_body.retention_days = 3;
        let EventsReplace::Applied(events) = replace_events(
            &source,
            &operator,
            session.events.revision,
            &events_body,
            "migrate-test-events",
        )
        .await
        .unwrap() else {
            panic!("events replace did not apply");
        };
        assert_eq!(output.revision, 2);
        assert_eq!(events.revision, 2);

        let admin = source
            .create_user(UserRole::Administrator, Some("Ada"), None)
            .await
            .unwrap();
        let sam = source
            .create_user(UserRole::Member, Some("Sam"), None)
            .await
            .unwrap();
        let actor = verify_patron_administrator(&source, admin.id)
            .await
            .unwrap();
        let kids = PatronScope {
            rules: vec![ScopeRule {
                field: ScopeField::Genre,
                value: "Kids".into(),
                effect: RuleEffect::Include,
            }],
            ..Default::default()
        };
        let default_scope = set_default_patron_scope(&source, &actor, kids.clone())
            .await
            .unwrap();
        assert!(default_scope.revision > 1, "imported, then replaced");

        let key = "platform:bookclerk/migrate-fixture";
        let instance = ensure_plugin_instance(&source, &ConfigActor::Bootstrap, key)
            .await
            .unwrap();
        // Larger than one chunk on every adapter, so the archive spans rows.
        let archive: Vec<u8> = (0..300 * 1024u32)
            .map(|n| u8::try_from(n % 251).unwrap())
            .collect();
        let url = put_artifact_blob(&source, &archive).await.unwrap();
        let sha = url.strip_prefix("blob:sha256:").unwrap().to_string();

        // A host already opened the destination (a reload onto the new
        // backend does): its identity, node row and seeded documents are
        // replaced, not a reason to refuse.
        bootstrap_control_plane(&dest, files.path(), &scratch, None, &Config::default())
            .await
            .unwrap();
        let summary = migrate_library_backend(&source, &dest, &apply())
            .await
            .unwrap();
        assert_eq!(summary.tables["plugin_artifacts"], 1);
        assert_eq!(summary.tables["plugin_instance_defaults"], 1);
        assert_eq!(summary.tables["library_identity"], 1);

        // A different seed: stored documents win over it.
        let reopened =
            bootstrap_control_plane(&dest, files.path(), &scratch, None, &Config::default())
                .await
                .unwrap();
        assert_eq!(reopened.library_id, session.library_id);
        assert_eq!(reopened.node.node_id, session.node.node_id);
        assert_eq!(reopened.output, *output);
        assert_eq!(reopened.events, events);
        for namespace in [OUTPUT_NAMESPACE, EVENTS_NAMESPACE] {
            assert_eq!(
                audit_count(&dest, namespace).await.unwrap(),
                audit_count(&source, namespace).await.unwrap(),
                "{namespace} audit rows"
            );
            assert_eq!(
                change_count(&dest, namespace).await.unwrap(),
                change_count(&source, namespace).await.unwrap(),
                "{namespace} change rows"
            );
        }

        assert_eq!(
            load_default_patron_scope(&dest).await.unwrap(),
            Some(default_scope)
        );
        let assigned = assigned_patron_scope(&dest, sam.id).await.unwrap();
        assert!(!assigned.own, "Sam still follows the default");
        assert_eq!(assigned.scope, kids);

        let routed = default_instance_for_key(&dest, key).await.unwrap().unwrap();
        assert_eq!(routed.id, instance.id);
        assert_eq!(
            load_artifact_blob(&dest, &sha).await.unwrap(),
            Some(archive)
        );
    }

    /// Every table of the library schema is either copied or listed in
    /// [`LEFT_BEHIND_TABLES`], and both lists name only real tables.
    /// Archives stored under any profile's chunk size are re-cut to the
    /// destination's limits, and a destination that refuses the interactive
    /// transaction (every external adapter profile) fails before anything is
    /// written to it.
    #[tokio::test]
    async fn archives_follow_each_profile_and_gated_destinations_are_refused() {
        use crate::control_plane::chunk_bytes;
        use crate::test_support::{sqlite, Profile, INTERACTIVE_TRANSACTIONS_REFUSED};

        let archive: Vec<u8> = (0..20 * 1024u32)
            .map(|i| u8::try_from(i % 251).unwrap())
            .collect();
        for from in Profile::ALL {
            let source = sqlite::memory(from).await.into_store();
            let url = put_artifact_blob(&source, &archive).await.unwrap();
            let sha = url.strip_prefix("blob:sha256:").unwrap().to_owned();

            for to in Profile::ALL {
                let caps = to.capabilities();
                let dest = memory_store().await.with_db_capabilities(caps.clone());
                let summary = migrate_library_backend(&source, &dest, &apply())
                    .await
                    .unwrap();
                assert_eq!(summary.tables["plugin_artifacts"], 1, "{from:?} -> {to:?}");
                let header = plugin_artifacts::Entity::find_by_id(sha.clone())
                    .one(dest.db())
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(
                    usize::try_from(header.chunk_bytes).unwrap(),
                    chunk_bytes(&caps),
                    "{from:?} -> {to:?}"
                );
                assert_eq!(
                    load_artifact_blob(&dest, &sha).await.unwrap().as_deref(),
                    Some(archive.as_slice()),
                    "{from:?} -> {to:?}"
                );

                let gated = sqlite::memory(to).await;
                let err = migrate_library_backend(&source, gated.store(), &apply())
                    .await
                    .unwrap_err();
                assert!(
                    err.to_string().contains(INTERACTIVE_TRANSACTIONS_REFUSED),
                    "{from:?} -> gated {to:?}: {err}"
                );
                assert_eq!(
                    plugin_artifacts::Entity::find()
                        .count(gated.connection())
                        .await
                        .unwrap(),
                    0,
                    "{from:?} -> gated {to:?}"
                );
            }
        }
    }

    #[tokio::test]
    async fn every_table_is_copied_or_left_behind_on_purpose() {
        let store = memory_store().await;
        let rows = store
            .db()
            .query_all_raw(canonical(
                "SELECT name FROM sqlite_master WHERE type = 'table' \
                 AND name NOT LIKE 'sqlite_%'",
            ))
            .await
            .unwrap();
        let schema: BTreeSet<String> = rows
            .iter()
            .map(|row| only_column::<String>(row).unwrap())
            .collect();

        let copied: BTreeSet<String> = dry_run_counts(store.db())
            .await
            .unwrap()
            .into_keys()
            .collect();
        let left: BTreeSet<String> = LEFT_BEHIND_TABLES
            .iter()
            .map(|(table, _)| (*table).to_string())
            .collect();

        assert!(
            copied.is_disjoint(&left),
            "copied and left behind: {:?}",
            copied.intersection(&left).collect::<Vec<_>>()
        );
        let known: BTreeSet<String> = copied.union(&left).cloned().collect();
        assert_eq!(
            schema.difference(&known).collect::<Vec<_>>(),
            Vec::<&String>::new(),
            "tables neither copied nor in LEFT_BEHIND_TABLES"
        );
        assert_eq!(
            known.difference(&schema).collect::<Vec<_>>(),
            Vec::<&String>::new(),
            "listed tables missing from the schema"
        );
    }
}
