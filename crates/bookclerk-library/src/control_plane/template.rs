//! Turning a seeded library into a template that fresh installs copy.
//!
//! An image can ship a library built at image-build time (the hosted demo
//! does: `packaging/demo/bake-seed.sh`) so that each container starts from
//! seeded data instead of seeding it again. The library rows are shared by
//! every copy, so nothing that belongs to one install may stay in it: the
//! cluster secret fingerprint (it pins the data-encryption key in
//! `master.key`, which each copy mints for itself), cluster leases and node
//! heartbeats from the build. Sealed secrets and sign-in sessions would be
//! readable by everyone holding the image, so a library that has any is
//! refused rather than cleaned.

use bookclerk_plugin_abi::DbValue;

use super::batch::{self, exec, query, request};
use crate::error::{LibraryError, Result};
use crate::store::LibraryStore;

/// Tables that must be empty before a library can become a template: rows
/// in them are secrets or sign-in state of the install that wrote them.
const MUST_BE_EMPTY: [&str; 4] = [
    "encrypted_secrets",
    "operator_sessions",
    "portal_sessions",
    "claim_tickets",
];

/// What [`strip_install_state`] removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TemplateStrip {
    /// `cluster_identity` rows deleted (0 or 1); each holds the cluster id and
    /// the fingerprint of its key, never the key.
    pub identity_rows: u64,
    /// Cluster lease rows deleted.
    pub leases: u64,
    /// Node heartbeat rows deleted.
    pub node_status: u64,
}

/// Removes this install's own state from the library so that copies of it
/// can start as installs of their own.
///
/// Deletes the cluster secret fingerprint, so the first daemon of each copy
/// records the key it mints instead of refusing it as a mismatch, plus the
/// cluster leases and node heartbeats left by the build. Library data,
/// plugin instances, deployments and the node rows stay. Run it only on a
/// library no daemon has open.
///
/// # Errors
///
/// Returns an error, and changes nothing, when `encrypted_secrets`,
/// `operator_sessions`, `portal_sessions` or `claim_tickets` holds a row: a
/// template must never carry sealed secrets or sessions.
/// Returns the database error when a statement fails.
pub async fn strip_install_state(store: &LibraryStore) -> Result<TemplateStrip> {
    let counts = MUST_BE_EMPTY
        .iter()
        .map(|table| query(&format!("SELECT count(*) FROM {table}"), Vec::new(), 1))
        .collect();
    let reply = store
        .execute_host_batch(request("template-check", counts))
        .await?;
    for (index, table) in MUST_BE_EMPTY.iter().enumerate() {
        let rows = batch::rows(&reply, index)
            .first()
            .and_then(|row| row.values.first())
            .cloned();
        match rows {
            Some(DbValue::Int64(0)) => {}
            other => {
                return Err(LibraryError::Other(anyhow::anyhow!(
                    "refusing to make a template: {table} has rows ({other:?})"
                )))
            }
        }
    }
    let reply = store
        .execute_host_batch(request(
            &format!("template-strip-{}", uuid::Uuid::new_v4()),
            vec![
                exec("DELETE FROM cluster_identity", Vec::new()),
                exec("DELETE FROM cluster_leases", Vec::new()),
                exec("DELETE FROM node_status", Vec::new()),
                super::bump(super::LiveTopic::Nodes),
            ],
        ))
        .await?;
    Ok(TemplateStrip {
        identity_rows: batch::rows_affected(&reply, 0),
        leases: batch::rows_affected(&reply, 1),
        node_status: batch::rows_affected(&reply, 2),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A migrated in-memory library.
    async fn store() -> LibraryStore {
        LibraryStore::from_connection(
            bookclerk_plugin_database_sqlite::open_memory()
                .await
                .unwrap(),
        )
    }

    /// Runs one host statement.
    async fn run(store: &LibraryStore, sql: &str) {
        store
            .execute_host_batch(request("test", vec![exec(sql, Vec::new())]))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn the_secret_root_leases_and_heartbeats_are_removed() {
        let store = store().await;
        run(
            &store,
            "INSERT INTO cluster_identity (id, cluster_id, secret_fingerprint, schema_state, created_at) \
             VALUES (1, 'c', 'f', 's', 't')",
        )
        .await;
        run(
            &store,
            "INSERT INTO cluster_leases (name, holder, token, expires_at_ms, renewed_at) \
             VALUES ('test.lease', 'n', 1, 0, 't')",
        )
        .await;
        let stripped = strip_install_state(&store).await.unwrap();
        assert_eq!(stripped.identity_rows, 1);
        // The migration's own lease and the test's.
        assert!(stripped.leases >= 1, "{stripped:?}");
        assert_eq!(stripped.node_status, 0);
        // A second pass finds nothing left.
        let again = strip_install_state(&store).await.unwrap();
        assert_eq!(
            again,
            TemplateStrip {
                identity_rows: 0,
                leases: 0,
                node_status: 0
            }
        );
    }

    #[tokio::test]
    async fn a_library_with_a_sealed_secret_is_refused_unchanged() {
        let store = store().await;
        run(
            &store,
            "INSERT INTO cluster_identity (id, cluster_id, secret_fingerprint, schema_state, created_at) \
             VALUES (1, 'c', 'f', 's', 't')",
        )
        .await;
        store
            .execute_host_batch(request(
                "seal",
                vec![exec(
                    "INSERT INTO encrypted_secrets (kind, name, ciphertext, created_at, updated_at) \
                     VALUES (?, ?, ?, ?, ?)",
                    vec![
                        batch::text("integration"),
                        batch::text("token"),
                        DbValue::Bytes(vec![1, 2, 3]),
                        batch::text("t"),
                        batch::text("t"),
                    ],
                )],
            ))
            .await
            .unwrap();
        let err = strip_install_state(&store).await.unwrap_err();
        assert!(err.to_string().contains("encrypted_secrets"), "{err}");
        let reply = store
            .execute_host_batch(request(
                "left",
                vec![query(
                    "SELECT count(*) FROM cluster_identity",
                    Vec::new(),
                    1,
                )],
            ))
            .await
            .unwrap();
        assert_eq!(
            batch::rows(&reply, 0)[0].values.first(),
            Some(&DbValue::Int64(1)),
            "nothing was deleted"
        );
    }
}
