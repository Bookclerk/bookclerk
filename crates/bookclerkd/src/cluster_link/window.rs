//! On a window: reading the books a library holds for this install's person
//! (`control_plane::window_books`).
//!
//! One [`WindowReader`] runs beside each link's client. While the link's
//! "Use this computer to download" setting is off, it reads every page of
//! the person's books from the library (`window.books`) on each welcomed
//! connection, every [`WINDOW_POLL`], and when Settings → Library changes
//! the setting ([`notify_window_changed`]). Each page is stored as it
//! arrives; once the last one is in, the books the read no longer named are
//! dropped. A read cut short (a lost connection, a refusal) keeps what the
//! previous complete read left and tries again on the next trigger. While
//! the setting is on, the link's window books are cleared.

use std::sync::LazyLock;
use std::time::Duration;

use bookclerk_library::control_plane::{
    clear_window_books, finish_window_read, load_cluster_link, record_window_page,
    window_read_stamp,
};
use bookclerk_library::LibraryStore;
use bookclerk_node_client::HostClientHandle;
use tokio::sync::watch;

/// Between reads while connected.
pub const WINDOW_POLL: Duration = Duration::from_secs(1800);

/// Most pages one read takes, so a read always ends. The library's cursor
/// must move forward on every page, so this only stops a library that
/// keeps paging; it is far above what a person's books need even when the
/// library's database answers one row per page.
const MAX_PAGES_PER_READ: usize = 1_000_000;

/// Bumped when a link's "Use this computer to download" setting changes.
static WINDOW_CHANGED: LazyLock<watch::Sender<u64>> = LazyLock::new(|| watch::channel(0).0);

/// Tells every running window reader to read (or clear) now.
pub fn notify_window_changed() {
    WINDOW_CHANGED.send_modify(|generation| *generation = generation.wrapping_add(1));
}

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
    /// The link downloads (or is gone or inactive); its window books were
    /// cleared.
    NotAWindow,
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
        let mut changed = WINDOW_CHANGED.subscribe();
        loop {
            if *shutdown.borrow() {
                return;
            }
            if self.handle.is_connected() {
                match self.read_once().await {
                    Ok(WindowRead::NotAWindow) => {}
                    Ok(WindowRead::Read { stored, dropped }) => tracing::debug!(
                        cluster_id = %self.cluster_id,
                        stored,
                        dropped,
                        "read the books the library holds for this window's person"
                    ),
                    Err(err) => tracing::warn!(
                        cluster_id = %self.cluster_id,
                        error = %err,
                        "could not read the library's books for this window; retrying later"
                    ),
                }
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
                result = changed.changed() => {
                    if result.is_err() {
                        return;
                    }
                }
                () = tokio::time::sleep(WINDOW_POLL) => {}
            }
        }
    }

    /// One complete read of the person's books, or a clear when the link
    /// downloads.
    ///
    /// # Errors
    ///
    /// Returns an error when the library cannot be reached or refuses, or a
    /// local read or write fails; the rows of the previous complete read
    /// stay.
    pub async fn read_once(&self) -> anyhow::Result<WindowRead> {
        let link = load_cluster_link(&self.library, &self.cluster_id).await?;
        if !link.is_some_and(|link| link.is_active() && !link.download) {
            clear_window_books(&self.library, &self.cluster_id).await?;
            return Ok(WindowRead::NotAWindow);
        }
        let seen_at = window_read_stamp();
        let mut after = 0;
        let mut stored = 0;
        for _ in 0..MAX_PAGES_PER_READ {
            let (works, next) = self
                .handle
                .window_books(after)
                .await
                .map_err(|err| anyhow::anyhow!("window.books: {err:?}"))?;
            stored += record_window_page(&self.library, &self.cluster_id, &seen_at, &works).await?;
            match next {
                Some(next) => after = next,
                None => {
                    let dropped =
                        finish_window_read(&self.library, &self.cluster_id, &seen_at).await?;
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
