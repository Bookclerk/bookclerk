//! A clerk's own acquires take the copies its libraries already hold.
//!
//! When this install acquires one of its own titles, the acquire pipeline
//! first asks whether a library it reaches holds a copy
//! ([`bookclerk_acquire::LibraryCopies`]). [`CardCopies`] asks each library
//! this install holds an active library card for, over the card's clerk
//! socket: the [`super::ClerkReader`] serving that socket sends
//! `copy.grant` naming the edition by its store listing, and the library
//! answers only for an edition the card's person may take. The pipeline
//! then downloads the files through [`CardCopy`] with the grant's token
//! (sent to the granting library alone). Once the copy is downloaded,
//! before the title is stored, it is recorded in `link_imports` under the
//! library's cluster id (again harmlessly when already recorded), so a
//! clerk that later becomes a branch never shares the book back.
//!
//! A copy a library downloaded from the title's own store is preferred.
//! A copy from another store is offered only when no library has one from
//! the title's own, and the pipeline then keeps the better of that copy
//! and its own store download. A copy whose store the library does not
//! know is never offered. A library whose reader is not connected now is
//! not waited for.
//!
//! [`acquire_copies`] is what this install's acquires ask: the libraries
//! it joined as a branch first, then those it holds a card for, with a
//! copy from the title's own store preferred over both.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use bookclerk_acquire::{
    from_store, LibraryCopies, LibraryCopy, LibraryCopyError, LibraryCopyFile, LibraryCopyListing,
    LibraryCopySource,
};
use bookclerk_library::control_plane::{
    list_clerk_links, record_copy_taken_from_link, ClerkLink, TakenCopy,
};
use bookclerk_library::{FileProbe, LibraryStore};
use bookclerk_node_client::{fetch_copy_file, CopyFetchError, OwnerEndpoint};
use bookclerk_node_proto::{ClerkCopy, COPY_AUTH_SCHEME};
use tokio::sync::{mpsc, oneshot};

use crate::cluster_link::library_copies::{listed, LinkCopies};

/// Times one file's download is tried before the copy is given up: a cut
/// transfer keeps what arrived, so each further try resumes it.
const FETCH_TRIES: u32 = 3;

/// How long an acquire waits for a library to answer `copy.grant`.
const GRANT_TIMEOUT: Duration = Duration::from_secs(60);

/// Grants one reader may have waiting to be sent.
pub(super) const ASK_QUEUE: usize = 8;

/// One `copy.grant` an acquire asks a reader to send.
pub(super) struct GrantAsk {
    /// Store id of the edition's listing.
    pub(super) store: String,
    /// The listing's marketplace; empty for a store without marketplaces.
    pub(super) marketplace: String,
    /// The listing's product id.
    pub(super) product_id: String,
    /// Where the answer goes: the copy, or `None` when the library did not
    /// grant one.
    pub(super) reply: oneshot::Sender<Option<ClerkCopy>>,
}

impl GrantAsk {
    /// True when `granted` echoes this ask's listing.
    pub(super) fn answered_by(&self, granted: &ClerkCopy) -> bool {
        granted.store == self.store
            && granted.marketplace == self.marketplace
            && granted.product_id == self.product_id
    }
}

/// Where the reader of each library card takes `copy.grant` asks, by the
/// library's cluster id; only a reader whose socket is welcomed is here.
static READERS: LazyLock<Mutex<BTreeMap<String, mpsc::Sender<GrantAsk>>>> =
    LazyLock::new(|| Mutex::new(BTreeMap::new()));

/// A connected reader's place in [`READERS`]; dropping it takes the reader
/// out again (unless a newer reader of the same library replaced it).
pub(super) struct Registered {
    /// The library's cluster id.
    cluster_id: String,
    /// The reader's own sender, to tell it from a newer one.
    asks: mpsc::Sender<GrantAsk>,
}

/// Lists a connected reader of `cluster_id`, so acquires can ask it for
/// copies, until the answered value is dropped.
pub(super) fn register(cluster_id: &str, asks: mpsc::Sender<GrantAsk>) -> Registered {
    if let Ok(mut readers) = READERS.lock() {
        readers.insert(cluster_id.to_string(), asks.clone());
    }
    Registered {
        cluster_id: cluster_id.to_string(),
        asks,
    }
}

impl Drop for Registered {
    fn drop(&mut self) {
        if let Ok(mut readers) = READERS.lock() {
            if readers
                .get(&self.cluster_id)
                .is_some_and(|asks| asks.same_channel(&self.asks))
            {
                readers.remove(&self.cluster_id);
            }
        }
    }
}

/// The connected reader of `cluster_id`, if there is one.
pub(super) fn reader_of(cluster_id: &str) -> Option<mpsc::Sender<GrantAsk>> {
    READERS
        .lock()
        .ok()
        .and_then(|readers| readers.get(cluster_id).cloned())
}

/// Asks the libraries this install holds a card for for their copy of a
/// title.
pub(crate) struct CardCopies {
    /// This install's own library: its cards, and where a taken copy is
    /// recorded.
    library: LibraryStore,
}

impl CardCopies {
    /// Asks over the readers running in this process.
    pub(crate) fn new(library: LibraryStore) -> Self {
        Self { library }
    }
}

/// Asks the libraries this install joined as a branch, then those it holds
/// a library card for. A copy from the title's own store is preferred;
/// without one, the first copy from another store is offered.
struct InstallCopies {
    /// The branch links.
    links: LinkCopies,
    /// The library cards.
    cards: CardCopies,
}

#[async_trait::async_trait]
impl LibraryCopies for InstallCopies {
    async fn copy_of(&self, listing: LibraryCopyListing<'_>) -> Option<Box<dyn LibraryCopy>> {
        let from_link = self.links.copy_of(listing).await;
        if from_link
            .as_ref()
            .is_some_and(|copy| from_store(copy.source_store(), listing.store))
        {
            return from_link;
        }
        // No branch link has a copy from the title's own store: a card's
        // copy is preferred only when it is from that store.
        match (from_link, self.cards.copy_of(listing).await) {
            (Some(_), Some(card)) if from_store(card.source_store(), listing.store) => Some(card),
            (Some(link), _) => Some(link),
            (None, card) => card,
        }
    }
}

/// Who the acquires of `library` ask before a store: its branch links,
/// then its library cards.
pub(super) fn install_copies(library: &LibraryStore) -> Arc<dyn LibraryCopies> {
    Arc::new(InstallCopies {
        links: LinkCopies::new(library.clone()),
        cards: CardCopies::new(library.clone()),
    })
}

/// What the acquires of `library` ask before a store
/// ([`install_copies`]).
pub(crate) fn acquire_copies(library: &LibraryStore) -> LibraryCopySource {
    LibraryCopySource::new(install_copies(library))
}

/// Sends `ask` to `reader` and waits for the library's answer.
async fn ask_grant(
    reader: &mpsc::Sender<GrantAsk>,
    listing: LibraryCopyListing<'_>,
) -> Option<ClerkCopy> {
    let (reply, answer) = oneshot::channel();
    let ask = GrantAsk {
        store: listing.store.to_string(),
        marketplace: listing.marketplace.to_string(),
        product_id: listing.product_id.to_string(),
        reply,
    };
    // A reader with a full queue is busy; the title comes from its store.
    reader.try_send(ask).ok()?;
    // A reader that ends before the answer drops the reply: no copy.
    tokio::time::timeout(GRANT_TIMEOUT, answer)
        .await
        .ok()?
        .ok()
        .flatten()
}

#[async_trait::async_trait]
impl LibraryCopies for CardCopies {
    async fn copy_of(&self, listing: LibraryCopyListing<'_>) -> Option<Box<dyn LibraryCopy>> {
        let links = match list_clerk_links(&self.library).await {
            Ok(links) => links,
            Err(err) => {
                tracing::warn!(error = %err, "could not read this install's library cards");
                return None;
            }
        };
        // A copy from another store only replaces the store download when
        // it is the better one, so it is offered only when no library has
        // a copy from the title's own store.
        let mut other_store: Option<CardCopy> = None;
        for link in links.into_iter().filter(ClerkLink::is_active) {
            let Some(reader) = reader_of(&link.library_cluster_id) else {
                continue;
            };
            let endpoint = match OwnerEndpoint::new(&link.origin, link.spki_pin.as_deref()) {
                Ok(endpoint) => endpoint,
                Err(err) => {
                    tracing::warn!(
                        library = %link.library_cluster_id,
                        error = %err,
                        "a library card's address cannot be dialed"
                    );
                    continue;
                }
            };
            let Some(granted) = ask_grant(&reader, listing).await else {
                continue;
            };
            let same_store = from_store(granted.copy.source_store.as_deref(), listing.store);
            // A copy whose store the library does not know is never
            // offered, and one other-store copy is enough.
            if !same_store && (granted.copy.source_store.is_none() || other_store.is_some()) {
                continue;
            }
            bookclerk_config::register_secret(granted.token.expose());
            let copy = CardCopy {
                files: granted.files.iter().map(listed).collect(),
                granted,
                endpoint,
                library: self.library.clone(),
                cluster_id: link.library_cluster_id,
                plugin_key: listing.plugin_key.to_string(),
                account_id: listing.account_id.to_string(),
                marketplace: listing.marketplace.to_string(),
                product_id: listing.product_id.to_string(),
            };
            if same_store {
                return Some(Box::new(copy));
            }
            other_store = Some(copy);
        }
        other_store.map(|copy| Box::new(copy) as Box<dyn LibraryCopy>)
    }
}

/// One copy a library granted this install's card, and the title here it
/// is for.
struct CardCopy {
    /// The grant: what the copy is and the token of its downloads.
    granted: ClerkCopy,
    /// The grant's files as the pipeline lists them.
    files: Vec<LibraryCopyFile>,
    /// The granting library.
    endpoint: OwnerEndpoint,
    /// This install's own library.
    library: LibraryStore,
    /// Cluster id of the granting library.
    cluster_id: String,
    /// Plugin key of the title here.
    plugin_key: String,
    /// Store account of the title here.
    account_id: String,
    /// Marketplace of the title here.
    marketplace: String,
    /// Product id of the title here.
    product_id: String,
}

impl Drop for CardCopy {
    /// The token leaves the redaction registry (it was registered when
    /// granted); its own copy is zeroized with the grant.
    fn drop(&mut self) {
        bookclerk_config::unregister_secret(self.granted.token.expose());
    }
}

#[async_trait::async_trait]
impl LibraryCopy for CardCopy {
    fn source_store(&self) -> Option<&str> {
        self.granted.copy.source_store.as_deref()
    }

    fn probe(&self) -> FileProbe {
        let copy = &self.granted.copy;
        FileProbe {
            content_type: copy.content_type.clone(),
            bitrate_kbps: copy.bitrate_kbps,
            lossless: copy.lossless,
            source_store: copy.source_store.clone(),
        }
    }

    fn files(&self) -> &[LibraryCopyFile] {
        &self.files
    }

    async fn fetch_file(&self, index: usize, dest: &Path) -> Result<(), LibraryCopyError> {
        let size = self
            .granted
            .files
            .get(index)
            .map(|file| file.size)
            .ok_or_else(|| Box::new(CopyFetchError::NoSuchFile) as LibraryCopyError)?;
        let authorization = zeroize::Zeroizing::new(format!(
            "{COPY_AUTH_SCHEME} {}",
            self.granted.token.expose()
        ));
        let mut tries = 0;
        loop {
            tries += 1;
            match fetch_copy_file(
                &self.endpoint,
                &authorization,
                &self.granted.copy_id,
                index,
                size,
                dest,
            )
            .await
            {
                Ok(()) => return Ok(()),
                // What arrived is kept, so trying again resumes it.
                Err(CopyFetchError::Transfer(err)) if tries < FETCH_TRIES => {
                    tracing::debug!(
                        library = %self.cluster_id,
                        index,
                        error = %err,
                        "a copy download was cut; resuming"
                    );
                }
                Err(err) => return Err(Box::new(err)),
            }
        }
    }

    async fn taken(&self) -> Result<(), LibraryCopyError> {
        record_copy_taken_from_link(
            &self.library,
            &self.cluster_id,
            TakenCopy {
                plugin_key: &self.plugin_key,
                account_id: &self.account_id,
                marketplace: &self.marketplace,
                product_id: &self.product_id,
            },
        )
        .await
        .map_err(|err| Box::new(err) as LibraryCopyError)
    }
}
