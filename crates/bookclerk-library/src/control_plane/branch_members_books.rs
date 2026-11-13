//! "Acquire all of this branch's members' books": a library operator's
//! choice, per branch, to receive every book the branch lists to it, not
//! only its owner's.
//!
//! On the library, [`set_branch_members_books`] stores the choice on the
//! branch's node row. The branch learns it when its catalog sync next opens
//! (`catalog.session.members_books`, see
//! [`super::branch_catalog::begin_catalog`]); a change restarts the
//! branch's pass from the start, so books it now sends arrive and books it
//! no longer sends are withdrawn when that pass ends.
//!
//! On the branch, the catalog sync then also sends the books of every
//! store account it lists to that library
//! ([`super::OwnerShareFilter::load`]): the same listing rules as catalog
//! answers, so a member's books go up only while the branch lists its
//! members' books and the member did not opt out. Only books already
//! acquired on the branch are sent; the branch downloads nothing for this.

use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait as _, EntityTrait as _, QueryFilter as _};

use super::identity::NodeKind;
use crate::entities::nodes;
use crate::error::{LibraryError, Result};
use crate::store::LibraryStore;

/// Turns "Acquire all of this branch's members' books" on or off for
/// branch `node_id`.
///
/// A change also clears the branch's catalog cursor and bumps its catalog
/// fence: the pass under way is lost, and the branch's next catalog
/// session (at most one sync interval later) starts from the start with
/// the new choice. Setting the value it already has changes nothing.
///
/// # Arguments
///
/// * `node_id` - The branch's node id.
/// * `on` - The new choice.
///
/// # Returns
///
/// `Some(on)` once stored, or `None` when `node_id` is not a branch (host
/// node) of this library.
///
/// # Errors
///
/// Returns [`LibraryError::Unavailable`] when the branch's catalog fence
/// kept changing under the write, or the read or write error.
pub async fn set_branch_members_books(
    store: &LibraryStore,
    node_id: &str,
    on: bool,
) -> Result<Option<bool>> {
    let wanted = i64::from(on);
    for _ in 0..8 {
        let Some(row) = nodes::Entity::find_by_id(node_id.to_string())
            .one(store.db())
            .await
            .map_err(LibraryError::Orm)?
            .filter(|row| NodeKind::parse(&row.kind) == NodeKind::Host)
        else {
            return Ok(None);
        };
        if row.acquire_members_books == wanted {
            return Ok(Some(on));
        }
        let updated = nodes::Entity::update_many()
            .col_expr(nodes::Column::AcquireMembersBooks, Expr::value(wanted))
            .col_expr(nodes::Column::CatalogCursor, Expr::value(String::new()))
            .col_expr(
                nodes::Column::CatalogFence,
                Expr::value(row.catalog_fence.saturating_add(1)),
            )
            .filter(nodes::Column::NodeId.eq(node_id))
            .filter(nodes::Column::CatalogFence.eq(row.catalog_fence))
            .exec(store.db())
            .await
            .map_err(LibraryError::Orm)?;
        if updated.rows_affected == 1 {
            tracing::info!(
                node_id,
                acquire_members_books = on,
                "changed whether this library acquires a branch's members' books"
            );
            return Ok(Some(on));
        }
    }
    Err(LibraryError::Unavailable(
        "the branch's catalog fence kept changing".into(),
    ))
}

#[cfg(test)]
#[path = "branch_members_books_tests.rs"]
mod tests;
