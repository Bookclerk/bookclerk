//! A branch's own acquires take the copies its libraries already hold.
//!
//! When this install acquires one of its own titles, the acquire pipeline
//! first asks here ([`bookclerk_acquire::LibraryCopies`]) whether a library
//! it joined holds a copy. [`LinkCopies`] asks each active link whose
//! "Keep copies on this computer" setting is on and whose client is
//! connected: `copy.grant` over the node socket names the edition by its
//! store listing, and the library answers only for an edition this
//! branch's person may take. The pipeline then downloads the files
//! through [`LinkCopy`] (the token goes to the granting library alone)
//! and, before it stores any of them, the copy is recorded in `link_imports`
//! so the catalog sync never shares the book back to the library it came
//! from.
//!
//! The pipeline takes a copy the library downloaded from the title's own
//! store instead of fetching from the store. A copy from another store is
//! offered only when no library has one from the title's own, and the
//! pipeline then keeps the better of that copy and its own store download.
//! A copy whose store the library does not know is never offered.

use std::path::Path;

use bookclerk_acquire::{
    from_store, LibraryCopies, LibraryCopy, LibraryCopyError, LibraryCopyFile, LibraryCopyListing,
};
use bookclerk_library::control_plane::{
    list_library_links, record_copy_taken_from_link, LibraryLink, TakenCopy,
};
use bookclerk_library::{FileProbe, FileRole, LibraryStore};
use bookclerk_node_client::{CopyFetchError, GrantedCopy, HostClientHandle};
use bookclerk_node_proto::CopyRole;

/// Times one file's download is tried before the copy is given up: a cut
/// transfer keeps what arrived, so each further try resumes it.
const FETCH_TRIES: u32 = 3;

/// Finds the running client of a link, if it has one.
type HandleOf = dyn Fn(&LibraryLink) -> Option<HostClientHandle> + Send + Sync;

/// Asks the libraries this install joined for their copy of a title.
pub(crate) struct LinkCopies {
    /// This install's own library: its links, and where a taken copy is
    /// recorded.
    library: LibraryStore,
    /// The running client of a link.
    handle_of: Box<HandleOf>,
}

impl LinkCopies {
    /// Asks over the link clients running in this process.
    pub(crate) fn new(library: LibraryStore) -> Self {
        Self::over(library, |link| {
            super::link_handle(&link.parent_library_id, &link.remote_node_id)
        })
    }

    /// Asks over the client `handle_of` names for each link.
    pub(crate) fn over(
        library: LibraryStore,
        handle_of: impl Fn(&LibraryLink) -> Option<HostClientHandle> + Send + Sync + 'static,
    ) -> Self {
        Self {
            library,
            handle_of: Box::new(handle_of),
        }
    }
}

/// True when this install takes copies from the library `link` joins: the
/// link is active and "Keep copies on this computer" is on.
fn takes_copies(link: &LibraryLink) -> bool {
    link.is_active() && link.keep_copies
}

#[async_trait::async_trait]
impl LibraryCopies for LinkCopies {
    async fn copy_of(&self, listing: LibraryCopyListing<'_>) -> Option<Box<dyn LibraryCopy>> {
        let links = match list_library_links(&self.library).await {
            Ok(links) => links,
            Err(err) => {
                tracing::warn!(error = %err, "could not read this install's library links");
                return None;
            }
        };
        // A copy from another store only replaces the store download when
        // it is the better one, so it is offered only when no library has
        // a copy from the title's own store.
        let mut other_store = None;
        for link in links.into_iter().filter(takes_copies) {
            // A library that is not connected now is not waited for: the
            // title comes from its store instead.
            let Some(handle) = (self.handle_of)(&link).filter(HostClientHandle::is_connected)
            else {
                continue;
            };
            let granted = match handle
                .copy_grant(listing.store, listing.marketplace, listing.product_id)
                .await
            {
                Ok(Some(granted)) => granted,
                Ok(None) => continue,
                Err(err) => {
                    tracing::debug!(
                        library = %link.parent_library_id,
                        error = %err,
                        "the library did not answer for its copy"
                    );
                    continue;
                }
            };
            let same_store = from_store(granted.copy.source_store.as_deref(), listing.store);
            let known_store = granted.copy.source_store.is_some();
            let copy = LinkCopy {
                files: granted.files.iter().map(listed).collect(),
                granted,
                handle,
                library: self.library.clone(),
                library_id: link.parent_library_id,
                plugin_key: listing.plugin_key.to_string(),
                account_id: listing.account_id.to_string(),
                marketplace: listing.marketplace.to_string(),
                product_id: listing.product_id.to_string(),
            };
            if same_store {
                return Some(Box::new(copy));
            }
            if known_store && other_store.is_none() {
                other_store = Some(copy);
            }
        }
        other_store.map(|copy| Box::new(copy) as Box<dyn LibraryCopy>)
    }
}

/// A granted file as the acquire pipeline lists it.
pub(crate) fn listed(file: &bookclerk_node_proto::CopyFile) -> LibraryCopyFile {
    LibraryCopyFile {
        name: file.name.clone(),
        role: match file.role {
            CopyRole::Audio => FileRole::Audio,
            CopyRole::Part => FileRole::Part,
            CopyRole::Chapter => FileRole::Chapter,
            CopyRole::Pdf => FileRole::Pdf,
            CopyRole::Cover => FileRole::Cover,
            CopyRole::Sidecar => FileRole::Sidecar,
            CopyRole::Transcript => FileRole::Transcript,
            CopyRole::Epub => FileRole::Epub,
        },
        size: file.size,
    }
}

/// One copy a library granted this branch, and the title here it is for.
struct LinkCopy {
    /// The grant: what the copy is and the token of its downloads.
    granted: GrantedCopy,
    /// The grant's files as the pipeline lists them.
    files: Vec<LibraryCopyFile>,
    /// The client of the link to the granting library.
    handle: HostClientHandle,
    /// This install's own library.
    library: LibraryStore,
    /// Library id of the granting library.
    library_id: String,
    /// Plugin key of the title here.
    plugin_key: String,
    /// Store account of the title here.
    account_id: String,
    /// Marketplace of the title here.
    marketplace: String,
    /// Product id of the title here.
    product_id: String,
}

#[async_trait::async_trait]
impl LibraryCopy for LinkCopy {
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
        let mut tries = 0;
        loop {
            tries += 1;
            match self
                .handle
                .fetch_copy_file(&self.granted, index, dest)
                .await
            {
                Ok(()) => return Ok(()),
                // What arrived is kept, so trying again resumes it.
                Err(CopyFetchError::Transfer(err)) if tries < FETCH_TRIES => {
                    tracing::debug!(
                        library = %self.library_id,
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
            &self.library_id,
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
