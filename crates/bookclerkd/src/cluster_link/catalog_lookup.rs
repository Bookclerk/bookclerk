//! On a branch: answering the catalog questions of the library it joined
//! (`control_plane::catalog_lookup`).
//!
//! One [`CatalogAnswerer`] runs beside each link's client. On every
//! welcomed connection, on each `catalog.asked` wake, and every
//! [`LOOKUP_POLL`], it pulls the questions waiting for this branch
//! (`catalog.questions`), answers them from this install's own library
//! ([`answer_questions`]: the owner's books, and the members' while the
//! link's "List my members' books" setting is on and they did not opt
//! out), and sends the answers back (`catalog.found`). Questions name works
//! by identifier only, and answers name no account, user or other library.

use std::time::Duration;

use anyhow::Context as _;
use bookclerk_library::control_plane::{answer_questions, load_cluster_link};
use bookclerk_library::LibraryStore;
use bookclerk_node_client::HostClientHandle;
use tokio::sync::{broadcast, watch};

/// Between pulls while connected, in case a wake was missed.
pub const LOOKUP_POLL: Duration = Duration::from_secs(900);

/// Most question batches answered in one pass, so one pass ends even when
/// the library keeps asking.
const MAX_BATCHES_PER_PASS: usize = 20;

/// Answers one link's catalog questions.
pub struct CatalogAnswerer {
    /// This install's library.
    pub library: LibraryStore,
    /// The library this link joined.
    pub cluster_id: String,
    /// The link's client.
    pub handle: HostClientHandle,
}

impl CatalogAnswerer {
    /// Runs passes until `shutdown` turns true.
    pub async fn run(self, mut shutdown: watch::Receiver<bool>) {
        let mut connections = self.handle.connections();
        let mut wakes = self.handle.subscribe_wakes();
        loop {
            if *shutdown.borrow() {
                return;
            }
            if self.handle.is_connected() {
                match self.answer_once().await {
                    Ok(0) => {}
                    Ok(answered) => tracing::debug!(
                        cluster_id = %self.cluster_id,
                        answered,
                        "answered the library's catalog questions"
                    ),
                    Err(err) => tracing::warn!(
                        cluster_id = %self.cluster_id,
                        error = %err,
                        "could not answer the library's catalog questions; retrying later"
                    ),
                }
            }
            loop {
                tokio::select! {
                    changed = shutdown.changed() => {
                        if changed.is_err() {
                            return;
                        }
                        break;
                    }
                    changed = connections.changed() => {
                        if changed.is_err() {
                            return;
                        }
                        break;
                    }
                    wake = wakes.recv() => match wake {
                        Ok(message) if is_catalog_asked(&message) => break,
                        Ok(_) => {}
                        // Missed wakes may have asked; pull to be sure.
                        Err(broadcast::error::RecvError::Lagged(_)) => break,
                        Err(broadcast::error::RecvError::Closed) => return,
                    },
                    () = tokio::time::sleep(LOOKUP_POLL) => break,
                }
            }
        }
    }

    /// Pulls and answers question batches until none is left (or
    /// [`MAX_BATCHES_PER_PASS`]).
    ///
    /// # Returns
    ///
    /// How many questions were answered.
    ///
    /// # Errors
    ///
    /// Returns an error when the library cannot be reached or refuses, or
    /// a local read fails.
    pub async fn answer_once(&self) -> anyhow::Result<usize> {
        let mut answered = 0;
        for _ in 0..MAX_BATCHES_PER_PASS {
            let Some(link) = load_cluster_link(&self.library, &self.cluster_id).await? else {
                return Ok(answered);
            };
            if !link.is_active() {
                return Ok(answered);
            }
            let questions = self
                .handle
                .catalog_questions()
                .await
                .map_err(|err| anyhow::anyhow!("catalog.questions: {err:?}"))?;
            if questions.is_empty() {
                return Ok(answered);
            }
            let (answers, missing) =
                answer_questions(&self.library, &questions, link.list_members_books)
                    .await
                    .context("looking the questions up in this library")?;
            answered += questions.len();
            self.handle
                .catalog_found(answers, missing)
                .await
                .map_err(|err| anyhow::anyhow!("catalog.found: {err:?}"))?;
        }
        Ok(answered)
    }
}

/// True for a `catalog.asked` wake.
fn is_catalog_asked(message: &serde_json::Value) -> bool {
    message.get("type").and_then(serde_json::Value::as_str) == Some("catalog.asked")
}

#[cfg(test)]
#[path = "catalog_lookup_tests.rs"]
mod tests;
