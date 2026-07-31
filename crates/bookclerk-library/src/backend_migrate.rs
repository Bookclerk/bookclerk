//! Copy library rows between database backends (sqlite ↔ d1 ↔ postgres).
//!
//! Used when an operator switches `[database].plugin` and wants to move existing
//! library state to the new backend. This is always opt-in — config reload and
//! plugin enablement do not run it automatically.

use sea_orm::{ConnectionTrait, DatabaseConnection, EntityTrait, PaginatorTrait, TransactionTrait};

use crate::entities::{
    account_links, accounts, books, claim_tickets, embeddings, encrypted_secrets, ignored_titles,
    listening_progress, portal_identities, portal_sessions, saved_filters, title_requests,
    user_preferences, work_editions, works,
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
    pub tables: std::collections::BTreeMap<String, usize>,
    pub dry_run: bool,
}

impl BackendMigrateSummary {
    #[must_use]
    pub fn total_rows(&self) -> usize {
        self.tables.values().sum()
    }
}

/// Copy all library tables from `source` into `dest`.
///
/// The destination should be empty unless [`BackendMigrateOptions::force`] is set.
/// Runs in a single transaction on the destination when not dry-run.
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

    let txn = dest.begin().await.map_err(LibraryError::Orm)?;

    macro_rules! copy {
        ($entity:ident, $name:literal) => {{
            let n = copy_entity::<$entity::Entity>(source, &txn).await?;
            summary.tables.insert($name.into(), n);
        }};
    }

    // FK-safe order.
    copy!(accounts, "accounts");
    copy!(portal_identities, "portal_identities");
    copy!(works, "works");
    copy!(books, "books");
    copy!(ignored_titles, "ignored_titles");
    copy!(saved_filters, "saved_filters");
    copy!(claim_tickets, "claim_tickets");
    copy!(portal_sessions, "portal_sessions");
    copy!(account_links, "account_links");
    copy!(work_editions, "work_editions");
    copy!(listening_progress, "listening_progress");
    copy!(title_requests, "title_requests");
    copy!(embeddings, "embeddings");
    copy!(user_preferences, "user_preferences");
    copy!(encrypted_secrets, "encrypted_secrets");

    txn.commit().await.map_err(LibraryError::Orm)?;
    Ok(summary)
}

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

async fn dest_row_count(db: &DatabaseConnection) -> Result<usize> {
    Ok(dry_run_counts(db).await?.values().sum())
}

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
    count!(accounts, "accounts");
    count!(portal_identities, "portal_identities");
    count!(works, "works");
    count!(books, "books");
    count!(ignored_titles, "ignored_titles");
    count!(saved_filters, "saved_filters");
    count!(claim_tickets, "claim_tickets");
    count!(portal_sessions, "portal_sessions");
    count!(account_links, "account_links");
    count!(work_editions, "work_editions");
    count!(listening_progress, "listening_progress");
    count!(title_requests, "title_requests");
    count!(embeddings, "embeddings");
    count!(user_preferences, "user_preferences");
    count!(encrypted_secrets, "encrypted_secrets");
    Ok(tables)
}
