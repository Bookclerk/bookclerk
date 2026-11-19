//! "Books in other libraries" on this library: the pages of this library's
//! books a trusted branch reads (`control_plane::window_books`), each
//! edition marked where it is the branch owner's.
//!
//! | Frame | Answer |
//! |---|---|
//! | `window.books` | `window.page {works, next?}` or `error {code: "refused"}` |
//!
//! The person is this connection's authenticated node's owner; the frame
//! names no user, so the marks are always that person's.

use bookclerk_library::control_plane::window_books_page;
use bookclerk_library::LibraryStore;
use bookclerk_node_proto::{Body, Frame, WindowCursor};

use super::work::{error, HostContext};

/// `window.books`.
pub(super) async fn books(
    library: &LibraryStore,
    host: &HostContext,
    id: String,
    after: WindowCursor,
) -> bookclerk_library::Result<Frame> {
    match window_books_page(library, &host.node_id, &after).await? {
        Some((works, next)) => Ok(Frame::new(id, Body::WindowPage { works, next })),
        None => Ok(error(id, "refused", "not_a_branch")),
    }
}
