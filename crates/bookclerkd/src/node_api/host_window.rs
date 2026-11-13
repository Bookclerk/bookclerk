//! Window mode on this library: the pages of a branch owner's books a
//! window reads (`control_plane::window_books`).
//!
//! | Frame | Answer |
//! |---|---|
//! | `window.books` | `window.page {works, next?}` or `error {code: "refused"}` |
//!
//! The person is this connection's authenticated node's owner; the frame
//! names no user, so a window can only read its own person's books.

use bookclerk_library::control_plane::window_books_page;
use bookclerk_library::LibraryStore;
use bookclerk_node_proto::{Body, Frame};

use super::work::{error, HostContext};

/// `window.books`.
pub(super) async fn books(
    library: &LibraryStore,
    host: &HostContext,
    id: String,
    after: i64,
) -> bookclerk_library::Result<Frame> {
    if after < 0 {
        return Ok(error(id, "invalid", "negative_cursor"));
    }
    match window_books_page(library, &host.node_id, after).await? {
        Some((works, next)) => Ok(Frame::new(id, Body::WindowPage { works, next })),
        None => Ok(error(id, "refused", "not_a_branch")),
    }
}
