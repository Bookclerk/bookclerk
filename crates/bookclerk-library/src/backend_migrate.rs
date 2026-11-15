//! Copy library rows between database backends (sqlite ↔ d1 ↔ postgres).
//!
//! Used when an operator switches `[database].plugin` and wants to move existing
//! library state to the new backend. This is always opt-in — config reload and
//! plugin enablement do not run it automatically.

use sea_orm::{ConnectionTrait, DatabaseConnection, EntityTrait, PaginatorTrait, TransactionTrait};

use crate::entities::{
    account_credential_revisions, account_links, account_shares, accounts, books, claim_tickets,
    contributors, credits, edition_acquisitions, edition_files, edition_identifiers, editions,
    embeddings, encrypted_secrets, expressions, holdings, ignored_titles, job_temp_paths, jobs,
    listening_progress, listing_offers, member_account_rules, oidc_auth_codes, oidc_clients,
    oidc_refresh_tokens, oidc_rp_states, operator_sessions, portal_identities, portal_sessions,
    resolution_conflicts, resolution_overrides, saved_filters, security_audit_events, series,
    series_entries, store_listings, title_request_sources, title_requests, user_invites,
    user_preferences, user_title_state, users, webauthn_challenges, webauthn_credentials,
    work_identifiers, works,
};
use crate::error::{LibraryError, Result};

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
    pub tables: std::collections::BTreeMap<String, usize>,
    /// When true, counts rows without writing to the destination.
    pub dry_run: bool,
}

impl BackendMigrateSummary {
    /// Sums row counts across all tables in the summary.
    #[must_use]
    pub fn total_rows(&self) -> usize {
        self.tables.values().sum()
    }
}

/// Copy all library tables from `source` into `dest`.
///
/// The destination should be empty unless [`BackendMigrateOptions::force`] is set.
/// Runs in a single transaction on the destination when not dry-run.
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
    source: &DatabaseConnection,
    dest: &DatabaseConnection,
    opts: &BackendMigrateOptions,
) -> Result<BackendMigrateSummary> {
    if !opts.force && !opts.dry_run {
        let existing = dest_row_count(dest).await?;
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
        summary.tables = dry_run_counts(source).await?;
        return Ok(summary);
    }

    // The destination is a native engine connection this tool opened, not
    // the store's adapter session, so an interactive transaction is available.
    #[allow(clippy::disallowed_methods)]
    let txn = dest.begin().await.map_err(LibraryError::Orm)?;

    macro_rules! copy {
        ($entity:ident, $name:literal) => {{
            let n = copy_entity::<$entity::Entity>(source, &txn).await?;
            summary.tables.insert($name.into(), n);
        }};
    }

    // FK-safe order (users before portal_identities.user_id).
    copy!(users, "users");
    copy!(accounts, "accounts");
    copy!(portal_identities, "portal_identities");
    // Shows before their episodes (`works.parent_work_id`).
    summary
        .tables
        .insert("works".into(), copy_works(source, &txn).await?);
    copy!(work_identifiers, "work_identifiers");
    copy!(expressions, "expressions");
    copy!(editions, "editions");
    copy!(edition_identifiers, "edition_identifiers");
    copy!(contributors, "contributors");
    copy!(credits, "credits");
    copy!(series, "series");
    copy!(series_entries, "series_entries");
    copy!(store_listings, "store_listings");
    copy!(listing_offers, "listing_offers");
    copy!(holdings, "holdings");
    copy!(resolution_overrides, "resolution_overrides");
    copy!(resolution_conflicts, "resolution_conflicts");
    copy!(books, "books");
    copy!(edition_files, "edition_files");
    copy!(edition_acquisitions, "edition_acquisitions");
    copy!(ignored_titles, "ignored_titles");
    copy!(saved_filters, "saved_filters");
    copy!(claim_tickets, "claim_tickets");
    copy!(user_invites, "user_invites");
    copy!(portal_sessions, "portal_sessions");
    copy!(operator_sessions, "operator_sessions");
    copy!(account_links, "account_links");
    copy!(account_shares, "account_shares");
    copy!(account_credential_revisions, "account_credential_revisions");
    copy!(listening_progress, "listening_progress");
    copy!(title_requests, "title_requests");
    copy!(title_request_sources, "title_request_sources");
    copy!(user_title_state, "user_title_state");
    copy!(embeddings, "embeddings");
    copy!(user_preferences, "user_preferences");
    copy!(member_account_rules, "member_account_rules");
    copy!(encrypted_secrets, "encrypted_secrets");
    copy!(security_audit_events, "security_audit_events");
    copy!(oidc_clients, "oidc_clients");
    copy!(oidc_auth_codes, "oidc_auth_codes");
    copy!(oidc_refresh_tokens, "oidc_refresh_tokens");
    copy!(oidc_rp_states, "oidc_rp_states");
    copy!(webauthn_credentials, "webauthn_credentials");
    copy!(webauthn_challenges, "webauthn_challenges");
    copy!(jobs, "jobs");
    copy!(job_temp_paths, "job_temp_paths");

    txn.commit().await.map_err(LibraryError::Orm)?;
    Ok(summary)
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

/// Sums library-table row counts on the destination (used to refuse a non-empty target).
async fn dest_row_count(db: &DatabaseConnection) -> Result<usize> {
    Ok(dry_run_counts(db).await?.values().sum())
}

/// Counts rows per library table without writing (dry-run summary).
async fn dry_run_counts(
    source: &DatabaseConnection,
) -> Result<std::collections::BTreeMap<String, usize>> {
    let mut tables = std::collections::BTreeMap::new();
    macro_rules! count {
        ($entity:ident, $name:literal) => {{
            let n = $entity::Entity::find()
                .count(source)
                .await
                .map_err(LibraryError::Orm)? as usize;
            tables.insert($name.into(), n);
        }};
    }
    count!(users, "users");
    count!(accounts, "accounts");
    count!(portal_identities, "portal_identities");
    count!(works, "works");
    count!(work_identifiers, "work_identifiers");
    count!(expressions, "expressions");
    count!(editions, "editions");
    count!(edition_identifiers, "edition_identifiers");
    count!(contributors, "contributors");
    count!(credits, "credits");
    count!(series, "series");
    count!(series_entries, "series_entries");
    count!(store_listings, "store_listings");
    count!(listing_offers, "listing_offers");
    count!(holdings, "holdings");
    count!(resolution_overrides, "resolution_overrides");
    count!(resolution_conflicts, "resolution_conflicts");
    count!(books, "books");
    count!(edition_files, "edition_files");
    count!(edition_acquisitions, "edition_acquisitions");
    count!(ignored_titles, "ignored_titles");
    count!(saved_filters, "saved_filters");
    count!(claim_tickets, "claim_tickets");
    count!(user_invites, "user_invites");
    count!(portal_sessions, "portal_sessions");
    count!(operator_sessions, "operator_sessions");
    count!(account_links, "account_links");
    count!(account_shares, "account_shares");
    count!(account_credential_revisions, "account_credential_revisions");
    count!(listening_progress, "listening_progress");
    count!(title_requests, "title_requests");
    count!(title_request_sources, "title_request_sources");
    count!(user_title_state, "user_title_state");
    count!(embeddings, "embeddings");
    count!(user_preferences, "user_preferences");
    count!(member_account_rules, "member_account_rules");
    count!(encrypted_secrets, "encrypted_secrets");
    count!(security_audit_events, "security_audit_events");
    count!(oidc_clients, "oidc_clients");
    count!(oidc_auth_codes, "oidc_auth_codes");
    count!(oidc_refresh_tokens, "oidc_refresh_tokens");
    count!(oidc_rp_states, "oidc_rp_states");
    count!(webauthn_credentials, "webauthn_credentials");
    count!(webauthn_challenges, "webauthn_challenges");
    count!(jobs, "jobs");
    count!(job_temp_paths, "job_temp_paths");
    Ok(tables)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sea_orm::{ActiveModelTrait, ActiveValue::Set, IntoActiveModel};

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
    async fn episodes_stored_before_their_show_still_copy() {
        let source = bookclerk_plugin_database_sqlite::open_memory()
            .await
            .unwrap();
        let dest = bookclerk_plugin_database_sqlite::open_memory()
            .await
            .unwrap();
        // The episode's row comes first in storage order, then gains its
        // parent once the show exists.
        work("w_episode", "podcast").insert(&source).await.unwrap();
        work("w_show", "podcast").insert(&source).await.unwrap();
        let mut episode = works::Entity::find_by_id("w_episode")
            .one(&source)
            .await
            .unwrap()
            .unwrap()
            .into_active_model();
        episode.parent_work_id = Set(Some("w_show".into()));
        episode.update(&source).await.unwrap();

        let opts = BackendMigrateOptions {
            dry_run: false,
            force: false,
        };
        let summary = migrate_library_backend(&source, &dest, &opts)
            .await
            .unwrap();
        assert_eq!(summary.tables["works"], 2);
        let copied = works::Entity::find_by_id("w_episode")
            .one(&dest)
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
        assert!(migrate_library_backend(&source, &dest, &opts)
            .await
            .is_err());
    }
}
