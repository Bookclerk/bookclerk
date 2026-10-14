//! Node ownership: which user may administer a node.
//!
//! An operator assigns a node to a user (`nodes.owner_user_id`). That user
//! may then change the node's `node.runtime` document (budgets, drain,
//! display name) without operator authority, through a
//! [`ConfigActor::NodeOwner`] that only [`verify_node_owner`] builds.
//! Placement labels, deployments, and every other document stay operator
//! writes.

use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

use super::documents::{ConfigActor, VerifiedNodeOwner};
use super::identity::{list_nodes, load_node, NodeRecord};
use crate::entities::nodes;
use crate::error::{LibraryError, Result};
use crate::models::UserStatus;
use crate::store::LibraryStore;

/// Error for a node the user does not own. An unknown node reads the same,
/// so a non-owner cannot probe which node ids exist.
fn not_owned(node_id: &str) -> LibraryError {
    LibraryError::Forbidden(format!("node {node_id} is not owned by this user"))
}

/// Returns a [`ConfigActor::NodeOwner`] for `user_id` on `node_id` when the
/// node's stored owner is that user.
///
/// # Errors
///
/// Returns [`LibraryError::Forbidden`] when the node is unknown or owned by
/// someone else, and the read error when the lookup fails.
pub async fn verify_node_owner(
    store: &LibraryStore,
    user_id: i64,
    node_id: &str,
) -> Result<ConfigActor> {
    let node = match load_node(store, node_id).await {
        Ok(node) => node,
        Err(LibraryError::NotFound(_)) => return Err(not_owned(node_id)),
        Err(err) => return Err(err),
    };
    if node.owner_user_id != Some(user_id) {
        return Err(not_owned(node_id));
    }
    Ok(ConfigActor::NodeOwner(VerifiedNodeOwner {
        user_id,
        node_id: node.node_id,
        audit: format!("node-owner:{user_id}"),
    }))
}

/// Assigns `node_id` to `owner` (a user id), or clears its owner.
///
/// The owner must be an active user whose role may administer owned nodes
/// (Host, Administrator, or Owner); anyone else could not use the node
/// routes, so the assignment would grant nothing.
///
/// # Errors
///
/// Returns [`LibraryError::Forbidden`] unless the actor is an operator,
/// [`LibraryError::NotFound`] when the node or user is unknown,
/// [`LibraryError::Invalid`] when the user is disabled or their role cannot
/// administer nodes, and the write error when the update fails.
pub async fn set_node_owner(
    store: &LibraryStore,
    actor: &ConfigActor,
    node_id: &str,
    owner: Option<i64>,
) -> Result<NodeRecord> {
    if !matches!(actor, ConfigActor::Operator { .. }) {
        return Err(LibraryError::Forbidden(
            "unauthorized configuration write: operator authority is required".into(),
        ));
    }
    if let Some(user_id) = owner {
        let user = store
            .get_user(user_id)
            .await?
            .ok_or_else(|| LibraryError::NotFound(format!("user {user_id}")))?;
        if user.status != UserStatus::Active {
            return Err(LibraryError::Invalid(format!(
                "user {user_id} is disabled and cannot own a node"
            )));
        }
        if !user.role.can_administer_owned_nodes() {
            return Err(LibraryError::Invalid(format!(
                "user {user_id} is a {}; only a host, administrator, or owner can own a node",
                user.role
            )));
        }
    }
    let res = nodes::Entity::update_many()
        .col_expr(nodes::Column::OwnerUserId, Expr::value(owner))
        .filter(nodes::Column::NodeId.eq(node_id))
        .exec(store.db())
        .await
        .map_err(LibraryError::Orm)?;
    if res.rows_affected == 0 {
        return Err(LibraryError::NotFound(format!("node {node_id}")));
    }
    load_node(store, node_id).await
}

/// Nodes `user_id` owns, sorted by id.
///
/// # Errors
///
/// Returns an error when the read fails.
pub async fn nodes_owned_by(store: &LibraryStore, user_id: i64) -> Result<Vec<NodeRecord>> {
    Ok(list_nodes(store)
        .await?
        .into_iter()
        .filter(|node| node.owner_user_id == Some(user_id))
        .collect())
}
