//! On a branch: reading the books of the library a link joined
//! (`control_plane::window_books`).
//!
//! One [`WindowReader`] runs beside each link's client. It reads every page
//! of the library's books (`window.books`), each edition marked where it is
//! this install's person's, on each welcomed connection and every
//! [`WINDOW_POLL`]. Each page is stored as it arrives; once the last one is
//! in, the books the read no longer named are dropped. A read cut short (a
//! lost connection, a refusal) keeps what the previous complete read left
//! and tries again on the next trigger. A link that is gone or no longer
//! active has its rows cleared, and a read that finds the link gone
//! between pages discards what it wrote.
//!
//! The reader runs whatever the link's "Keep copies on this computer"
//! setting says: a branch always shows its library's books.

use std::time::Duration;

use bookclerk_library::control_plane::{
    clear_window_books, discard_window_read, finish_window_read, load_cluster_link,
    record_window_page, window_read_stamp,
};
use bookclerk_library::LibraryStore;
use bookclerk_node_client::HostClientHandle;
use tokio::sync::watch;

/// Between reads while connected.
pub const WINDOW_POLL: Duration = Duration::from_secs(1800);

/// Most pages one read takes, so a read always ends. The library's cursor
/// must move forward on every page, so this only stops a library that
/// keeps paging; it is far above what a library's books need even when the
/// library's database answers one row per page.
const MAX_PAGES_PER_READ: usize = 1_000_000;

/// Reads one link's window books.
pub struct WindowReader {
    /// This install's library.
    pub library: LibraryStore,
    /// The library this link joined.
    pub cluster_id: String,
    /// The link's client.
    pub handle: HostClientHandle,
}

/// What one [`WindowReader::read_once`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowRead {
    /// The link is gone or no longer active; its window books were
    /// cleared.
    Inactive,
    /// The link went while the read ran: what it wrote was discarded.
    Superseded,
    /// A complete read stored this many editions and dropped this many rows
    /// the library no longer names.
    Read {
        /// Editions stored.
        stored: usize,
        /// Rows dropped.
        dropped: u64,
    },
}

impl WindowReader {
    /// Runs reads until `shutdown` turns true.
    pub async fn run(self, mut shutdown: watch::Receiver<bool>) {
        let mut connections = self.handle.connections();
        loop {
            if *shutdown.borrow() {
                return;
            }
            if self.handle.is_connected() {
                match self.read_once().await {
                    Ok(WindowRead::Inactive | WindowRead::Superseded) => {}
                    Ok(WindowRead::Read { stored, dropped }) => tracing::debug!(
                        cluster_id = %self.cluster_id,
                        stored,
                        dropped,
                        "read the library's books for this branch"
                    ),
                    Err(err) => tracing::warn!(
                        cluster_id = %self.cluster_id,
                        error = %err,
                        "could not read the library's books for this branch; retrying later"
                    ),
                }
            } else if let Err(err) = self.clear_if_inactive().await {
                tracing::warn!(
                    cluster_id = %self.cluster_id,
                    error = %err,
                    "could not clear a departed link's window books; retrying later"
                );
            }
            tokio::select! {
                result = shutdown.changed() => {
                    if result.is_err() {
                        return;
                    }
                }
                result = connections.changed() => {
                    if result.is_err() {
                        return;
                    }
                }
                () = tokio::time::sleep(WINDOW_POLL) => {}
            }
        }
    }

    /// Clears the link's window books when it is gone or no longer
    /// active. Needs no connection.
    ///
    /// # Returns
    ///
    /// True when the link is gone or inactive (its books are cleared).
    ///
    /// # Errors
    ///
    /// Returns a local read or write error.
    pub async fn clear_if_inactive(&self) -> anyhow::Result<bool> {
        if self.is_active().await? {
            return Ok(false);
        }
        clear_window_books(&self.library, &self.cluster_id).await?;
        Ok(true)
    }

    /// True while the link is recorded here and active.
    async fn is_active(&self) -> anyhow::Result<bool> {
        Ok(load_cluster_link(&self.library, &self.cluster_id)
            .await?
            .is_some_and(|link| link.is_active()))
    }

    /// One complete read of the library's books, or a clear when the link
    /// is gone or inactive.
    ///
    /// # Errors
    ///
    /// Returns an error when the library cannot be reached or refuses, or a
    /// local read or write fails; the rows of the previous complete read
    /// stay.
    pub async fn read_once(&self) -> anyhow::Result<WindowRead> {
        if self.clear_if_inactive().await? {
            return Ok(WindowRead::Inactive);
        }
        self.read_pages().await
    }

    /// The paging part of [`Self::read_once`]. When the link goes during
    /// the read, everything the read wrote is discarded.
    ///
    /// # Errors
    ///
    /// As [`Self::read_once`].
    async fn read_pages(&self) -> anyhow::Result<WindowRead> {
        let seen_at = window_read_stamp();
        let mut after = bookclerk_node_proto::WindowCursor::start();
        let mut stored = 0;
        for _ in 0..MAX_PAGES_PER_READ {
            let (works, next) = self
                .handle
                .window_books(&after)
                .await
                .map_err(|err| anyhow::anyhow!("window.books: {err:?}"))?;
            stored += record_window_page(&self.library, &self.cluster_id, &seen_at, &works).await?;
            if !self.is_active().await? {
                discard_window_read(&self.library, &self.cluster_id, &seen_at).await?;
                return Ok(WindowRead::Superseded);
            }
            match next {
                Some(next) => after = next,
                None => {
                    let dropped =
                        finish_window_read(&self.library, &self.cluster_id, &seen_at).await?;
                    if !self.is_active().await? {
                        discard_window_read(&self.library, &self.cluster_id, &seen_at).await?;
                        return Ok(WindowRead::Superseded);
                    }
                    if stored > 0 || dropped > 0 {
                        crate::live::poke(bookclerk_library::control_plane::LiveTopic::Clusters);
                    }
                    return Ok(WindowRead::Read { stored, dropped });
                }
            }
        }
        anyhow::bail!("the library kept paging past {MAX_PAGES_PER_READ} pages")
    }
}

#[cfg(test)]
#[path = "window_tests.rs"]
mod tests;
