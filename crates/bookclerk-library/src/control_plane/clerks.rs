//! Clerks: owner nodes (`kind = owner`) that joined a library with a
//! sign-in an Owner approved as such, sharing its database and DEK.
//!
//! - [`library_contents`] is what a joining install checks before it may
//!   join as a clerk: a clerk's own database is replaced by the library's,
//!   so the install must hold no library data of its own (no users, store
//!   accounts, books, or libraries it is a branch of).
//! - [`remove_owner_node`] removes an owner node that is no longer running
//!   from the library's node list, the way any owner node leaves.

use chrono::Utc;
use sea_orm::{ColumnTrait, EntityTrait, PaginatorTrait, QueryFilter};
use serde::Serialize;

use super::cluster_links::CLUSTER_LINK_LEFT;
use super::enrollment::EnrollmentActor;
use super::identity::{load_node, NodeKind};
use crate::entities::{accounts, books, cluster_links, node_status, nodes, users};
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
        let mut out = Vec::new();
        for (count, what) in [
            (self.users, "user"),
            (self.accounts, "store account"),
            (self.books, "book"),
            (self.libraries_joined, "library it is a branch of"),
        ] {
            if count > 0 {
                out.push(format!(
                    "{count} {what}{}",
                    if count == 1 || what.starts_with("library") {
                        ""
                    } else {
                        "s"
                    }
                ));
            }
        }
        out
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

/// Removes owner node `node_id` from the library's node list: its `nodes`
/// row (and with it its labels and status). Only a node that is not
/// running may go, and never `this_node`: a running owner node writes its
/// row back at its next heartbeat. Removing a node does not take back the
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
    if super::load_node_status(store, node_id)
        .await?
        .is_some_and(|status| status.is_live(now_ms))
    {
        return Err(LibraryError::Conflict(format!(
            "node {node_id} is running; stop Bookclerk there first"
        )));
    }
    node_status::Entity::delete_many()
        .filter(node_status::Column::NodeId.eq(node_id))
        .exec(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    nodes::Entity::delete_many()
        .filter(nodes::Column::NodeId.eq(node_id))
        .filter(nodes::Column::Kind.eq(NodeKind::Owner.as_str()))
        .exec(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    let detail = serde_json::json!({ "node_id": node_id }).to_string();
    let _ = store
        .insert_security_audit_event(&audit, "owner_node_removed", Some(&detail))
        .await;
    tracing::info!(node_id, "removed an owner node from the library");
    Ok(())
}
