//! Clerks: owner nodes (`kind = owner`) that joined a library with a
//! sign-in an Owner approved as such, sharing its database and DEK.
//!
//! - [`library_contents`] is what a joining install checks before it may
//!   join as a clerk: a clerk's own database is replaced by the library's,
//!   so the install must hold no library data of its own (no users, store
//!   accounts, books, or libraries it is a branch of).
//! - [`owner_nodes_visible_to`] lists the owner nodes a caller may see
//!   (Settings → Clerks), by the same rule [`remove_owner_node`] removes
//!   them.
//! - [`remove_owner_node`] removes an owner node that is no longer running
//!   from the library's node list, the way any owner node leaves.

use chrono::Utc;
use sea_orm::{ColumnTrait, EntityTrait, PaginatorTrait, QueryFilter};
use serde::Serialize;

use super::batch;
use super::cluster_links::CLUSTER_LINK_LEFT;
use super::enrollment::EnrollmentActor;
use super::identity::{list_nodes, load_node, NodeKind, NodeRecord};
use crate::entities::{accounts, books, cluster_links, users};
use crate::error::{LibraryError, Result};
use crate::models::{UserRole, UserStatus};
use crate::store::LibraryStore;

/// What library data an install holds of its own.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct LibraryContents {
    /// Users of any role.
    pub users: u64,
    /// Store accounts.
    pub accounts: u64,
    /// Books.
    pub books: u64,
    /// Libraries this install is a branch of (not left).
    pub libraries_joined: u64,
}

impl LibraryContents {
    /// True when the install holds no library data: it may join as a clerk.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Why the install is not empty, for the operator.
    #[must_use]
    pub fn reasons(&self) -> Vec<String> {
        [
            (self.users, "user", "users"),
            (self.accounts, "store account", "store accounts"),
            (self.books, "book", "books"),
            (
                self.libraries_joined,
                "library it is a branch of",
                "libraries it is a branch of",
            ),
        ]
        .into_iter()
        .filter(|(count, _, _)| *count > 0)
        .map(|(count, one, many)| format!("{count} {}", if count == 1 { one } else { many }))
        .collect()
    }
}

/// Counts the library data this install holds of its own.
///
/// # Errors
///
/// Returns the read error.
pub async fn library_contents(store: &LibraryStore) -> Result<LibraryContents> {
    let db = store.db();
    Ok(LibraryContents {
        users: users::Entity::find()
            .count(db)
            .await
            .map_err(LibraryError::Orm)?,
        accounts: accounts::Entity::find()
            .count(db)
            .await
            .map_err(LibraryError::Orm)?,
        books: books::Entity::find()
            .count(db)
            .await
            .map_err(LibraryError::Orm)?,
        libraries_joined: cluster_links::Entity::find()
            .filter(cluster_links::Column::State.ne(CLUSTER_LINK_LEFT))
            .count(db)
            .await
            .map_err(LibraryError::Orm)?,
    })
}

/// The owner nodes (`kind = owner`: this one and its clerks) `actor` may
/// see, by the rule [`remove_owner_node`] removes them: the operator or an
/// active Owner sees every one; another active user only the ones they own;
/// an unknown or disabled user none.
///
/// # Errors
///
/// Returns the read error.
pub async fn owner_nodes_visible_to(
    store: &LibraryStore,
    actor: &EnrollmentActor,
) -> Result<Vec<NodeRecord>> {
    let owner_nodes = list_nodes(store)
        .await?
        .into_iter()
        .filter(|node| node.kind == NodeKind::Owner);
    match actor {
        EnrollmentActor::Operator { .. } => Ok(owner_nodes.collect()),
        EnrollmentActor::User { user_id } => {
            let user = store
                .get_user(*user_id)
                .await?
                .filter(|user| user.status == UserStatus::Active);
            Ok(match user {
                None => Vec::new(),
                Some(user) if user.role == UserRole::Owner => owner_nodes.collect(),
                Some(user) => owner_nodes
                    .filter(|node| node.owner_user_id == Some(user.id))
                    .collect(),
            })
        }
    }
}

/// Removes owner node `node_id` from the library's node list, in one
/// transaction: its `nodes` row (and with it its labels and credentials),
/// status, event-subscriber registrations, secret handoffs, and plugin
/// deployments with their observations. Only a node that is not
/// running may go, and never `this_node`. The liveness check and the
/// removal are one step: the node's status row is deleted only if it still
/// holds the stale heartbeat that was checked, so a heartbeat that lands
/// in between refuses the removal. A node that was only cut off from the
/// database for longer than its liveness window can still be removed while
/// it runs; its next heartbeat writes a status row again, but not its node
/// row. Removing a node does not take back the
/// database credentials or DEK it held; rotate those when it is not
/// trusted any more.
///
/// The operator or an Owner may remove any owner node; another user only
/// one they own.
///
/// # Errors
///
/// Returns [`LibraryError::NotFound`] for an unknown node or one the
/// caller may not see, [`LibraryError::Invalid`] for a host node (revoke
/// it instead) or this node, [`LibraryError::Conflict`] while it is live,
/// and the write error.
pub async fn remove_owner_node(
    store: &LibraryStore,
    actor: &EnrollmentActor,
    node_id: &str,
    this_node: &str,
) -> Result<()> {
    let not_found = || LibraryError::NotFound(format!("node {node_id}"));
    let node = match load_node(store, node_id).await {
        Ok(node) => node,
        Err(LibraryError::NotFound(_)) => return Err(not_found()),
        Err(err) => return Err(err),
    };
    let audit = match actor {
        EnrollmentActor::Operator { id } => format!("operator:{id}"),
        EnrollmentActor::User { user_id } => {
            let user = store
                .get_user(*user_id)
                .await?
                .filter(|user| user.status == UserStatus::Active)
                .ok_or_else(not_found)?;
            if user.role != UserRole::Owner && node.owner_user_id != Some(user.id) {
                return Err(not_found());
            }
            format!("user:{}", user.id)
        }
    };
    if node.kind != NodeKind::Owner {
        return Err(LibraryError::Invalid(format!(
            "node {node_id} is a branch: revoke it instead"
        )));
    }
    if node_id == this_node {
        return Err(LibraryError::Invalid(
            "this node cannot remove itself; stop it, then remove it from another node".into(),
        ));
    }
    let now_ms = Utc::now().timestamp_millis();
    let running = || {
        LibraryError::Conflict(format!(
            "node {node_id} is running; stop Bookclerk there first"
        ))
    };
    let checked = super::load_node_status(store, node_id).await?;
    if checked
        .as_ref()
        .is_some_and(|status| status.is_live(now_ms))
    {
        return Err(running());
    }
    // The node's rows go together. These tables are keyed by node id with
    // no foreign key to `nodes` (only `node_credentials` and
    // `node_upload_sessions` cascade), so each is cleared here: the node's
    // heartbeat, event-subscriber registrations, secret handoffs, plugin
    // deployments and their observations (its own, and other nodes' of its
    // deployments). Node-scoped configuration documents and their audit
    // history are kept, as the configuration audit trail is.
    //
    // The deletes go to the database plugin as one atomic batch. Every
    // delete is guarded by the heartbeat checked above: with a status row,
    // the row must still hold that heartbeat; without one, none may have
    // appeared. A node that beat in the meantime makes every guard false,
    // so the batch deletes nothing and the node stays.
    let guard = match &checked {
        Some(_) => "EXISTS (SELECT 1 FROM node_status WHERE node_id = ? AND heartbeat_ms = ?)",
        None => "NOT EXISTS (SELECT 1 FROM node_status WHERE node_id = ?)",
    };
    let guard_binds = || -> Vec<bookclerk_plugin_abi::DbValue> {
        match &checked {
            Some(status) => vec![batch::text(node_id), batch::int(status.heartbeat_ms)],
            None => vec![batch::text(node_id)],
        }
    };
    let guarded = |sql: &str, mut binds: Vec<bookclerk_plugin_abi::DbValue>| {
        binds.extend(guard_binds());
        batch::exec(&format!("{sql} AND {guard}"), binds)
    };
    let mut statements = vec![
        guarded(
            "DELETE FROM event_subscriber_nodes WHERE node_id = ?",
            vec![batch::text(node_id)],
        ),
        guarded(
            "DELETE FROM job_secret_handoffs WHERE node_id = ?",
            vec![batch::text(node_id)],
        ),
        guarded(
            "DELETE FROM plugin_deployment_observations \
             WHERE (node_id = ? OR deployment_id IN \
                 (SELECT deployment_id FROM plugin_deployments WHERE node_id = ?))",
            vec![batch::text(node_id), batch::text(node_id)],
        ),
        guarded(
            "DELETE FROM plugin_deployments WHERE node_id = ?",
            vec![batch::text(node_id)],
        ),
        guarded(
            "DELETE FROM nodes WHERE node_id = ? AND kind = ?",
            vec![batch::text(node_id), batch::text(NodeKind::Owner.as_str())],
        ),
    ];
    /// Index of the `nodes` delete in the batch.
    const NODES_DELETE: usize = 4;
    // Compare and delete: the status row goes last, and only while it still
    // holds the heartbeat checked above.
    if let Some(status) = &checked {
        statements.push(batch::exec(
            "DELETE FROM node_status WHERE node_id = ? AND heartbeat_ms = ?",
            vec![batch::text(node_id), batch::int(status.heartbeat_ms)],
        ));
    }
    let operation_id = format!("owner-node-remove-{}", uuid::Uuid::new_v4());
    let reply = store
        .execute_host_batch(batch::request(&operation_id, statements))
        .await?;
    // With a checked row it must be the one deleted; with none, a row that
    // appeared since is a first heartbeat, so the node is starting and the
    // guards kept its rows.
    let removed = match &checked {
        Some(_) => batch::rows_affected(&reply, NODES_DELETE + 1) == 1,
        None => batch::rows_affected(&reply, NODES_DELETE) == 1,
    };
    if !removed {
        return Err(running());
    }
    let detail = serde_json::json!({ "node_id": node_id }).to_string();
    if let Err(err) = store
        .insert_security_audit_event(&audit, "owner_node_removed", Some(&detail))
        .await
    {
        tracing::warn!(
            node_id,
            error = %err,
            "removed an owner node, but could not record the owner_node_removed audit event"
        );
    }
    tracing::info!(node_id, "removed an owner node from the library");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasons_use_each_entrys_own_singular_and_plural() {
        let one = LibraryContents {
            users: 1,
            accounts: 1,
            books: 1,
            libraries_joined: 1,
        };
        assert_eq!(
            one.reasons(),
            [
                "1 user",
                "1 store account",
                "1 book",
                "1 library it is a branch of"
            ]
        );
        let many = LibraryContents {
            users: 2,
            accounts: 0,
            books: 3,
            libraries_joined: 2,
        };
        assert_eq!(
            many.reasons(),
            ["2 users", "3 books", "2 libraries it is a branch of"]
        );
    }
}
