//! On a branch: sharing this library's books with the library it joined
//! (branches of branches).
//!
//! One [`CatalogSync`] runs beside each link's client. On every welcomed
//! connection, every [`SYNC_INTERVAL`], and when Settings → Library changes
//! a setting ([`notify_sharing_changed`]), it:
//!
//! 1. sends `catalog.begin` with the link's two settings ("Share this
//!    branch's books with *library*", "Include books from my own
//!    branches"); the library answers a fresh fence, where to resume, its
//!    ancestry (recorded on the link, so `info` and the join cycle check
//!    stay current), its destination ids, and titles whose files it still
//!    wants;
//! 2. with sharing on, sends the titles after the cursor in fenced
//!    `catalog.sync` batches, in a stable order (last change, then id): the
//!    branch owner's own books acquired on this install (held by a store
//!    account connected here to its Owner, and not imported from that
//!    library, [`OwnerShareFilter`]) and, with the second setting
//!    on, the titles this install's own branches shared with it and whose
//!    files arrived here. A title carries store, product id, metadata and
//!    its file manifest only ([`bookclerk_node_proto::CatalogTitle`]): no
//!    store account, user, or downstream branch is named;
//! 3. streams the files of every wanted title from this install's primary
//!    destination to the library's upload endpoint (`catalog.upload`, one
//!    `PUT` per file, `commit`), which writes them straight to the
//!    library's destinations under the same keys.
//!
//! The library keeps the cursor and the undelivered titles, so a sync cut
//! by a disconnect resumes where it stopped. Nothing of the library's work
//! is sent onward: this module only reads this install's own library.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::sync::Arc;
use std::sync::LazyLock;
use std::time::Duration;

use anyhow::Context as _;
use bookclerk_library::control_plane::{
    forwardable_branch_title, forwardable_branch_title_keys_page, load_cluster_link,
    set_cluster_link_ancestry, ForwardableBranchKey, ForwardableBranchTitle, OwnerShareFilter,
    FORWARDABLE_KEYS_MAX_PAGE,
};
use bookclerk_library::{
    AcquireStatus, AcquiredBookKey, BookRecord, LibraryStore, ACQUIRED_KEYS_MAX_PAGE,
};
use bookclerk_node_client::{CatalogSession, HostClientHandle, OwnerCallError};
use bookclerk_node_proto::{
    is_storage_key, CatalogFile, CatalogMetadata, CatalogRef, CatalogTitle, FileCopy, UploadFile,
    MAX_CATALOG_BATCH_TITLES, MAX_UPLOAD_FILES,
};
use bookclerk_storage::StorageBackend;
use sha2::Digest as _;
use tokio::io::AsyncReadExt as _;
use tokio::sync::watch;

use crate::api::AppState;

/// Between catalog passes while connected.
pub const SYNC_INTERVAL: Duration = Duration::from_secs(300);

/// Identity rows per keyset read while listing a pass's titles: the
/// readers' maximum, or two under test so every walk crosses pages.
const ENTRY_PAGE: u64 = if cfg!(test) {
    2
} else {
    ACQUIRED_KEYS_MAX_PAGE
};

/// Identity rows per keyset read of branch titles, as [`ENTRY_PAGE`].
const BRANCH_ENTRY_PAGE: u64 = if cfg!(test) {
    2
} else {
    FORWARDABLE_KEYS_MAX_PAGE
};

/// Largest serialized `catalog.sync` batch (well under a frame).
const MAX_BATCH_BYTES: usize = 768 * 1024;

/// Bumped when a sharing setting changes; every catalog sync in this
/// process watches it (a watch, so a change made mid-pass is not missed).
static SHARING_CHANGED: LazyLock<watch::Sender<u64>> = LazyLock::new(|| watch::channel(0).0);

/// Tells every running catalog sync to start a pass now.
pub fn notify_sharing_changed() {
    SHARING_CHANGED.send_modify(|generation| *generation = generation.wrapping_add(1));
}

/// Where this install's own files are.
pub trait BranchShelf: Send + Sync + 'static {
    /// This install's primary output destination, when one is deployed.
    fn primary(&self) -> impl Future<Output = Option<Box<dyn StorageBackend>>> + Send;
}

/// The daemon's [`BranchShelf`]: its deployed destinations.
pub struct DaemonShelf {
    /// Daemon state.
    pub state: Arc<AppState>,
}

impl BranchShelf for DaemonShelf {
    async fn primary(&self) -> Option<Box<dyn StorageBackend>> {
        let cfg = self.state.config.read().await.clone();
        bookclerk_plugin_host::build_acquire_destinations(
            &cfg,
            &*self.state.destinations.read().await,
        )
        .ok()?
        .items
        .into_iter()
        .next()
        .map(|item| item.backend)
    }
}

/// What one pass did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncReport {
    /// Sharing was on.
    pub shared: bool,
    /// Titles sent.
    pub sent: u64,
    /// Titles the library recorded.
    pub upserted: u64,
    /// Titles the library already held.
    pub skipped: u64,
    /// Titles whose files were delivered.
    pub delivered: u64,
}

/// One title this install may share, by identity only: its full row is
/// read when the title is sent or delivered ([`CatalogSync::load`]), so a
/// pass never holds every title's metadata at once.
#[derive(Debug, Clone)]
enum Source {
    /// A book acquired on this install.
    Own(AcquiredBookKey),
    /// A title one of this install's branches shared, files delivered.
    Branch(ForwardableBranchKey),
}

/// The full row behind a [`Source`], read just before it is used.
#[derive(Debug, Clone)]
enum Loaded {
    /// A book acquired on this install.
    Own(Box<BookRecord>),
    /// A title one of this install's branches shared, files delivered.
    Branch(Box<ForwardableBranchTitle>),
}

/// A shareable title and its place in the sync order.
#[derive(Debug, Clone)]
struct Entry {
    /// Sort key and cursor: last change, kind, id.
    order: String,
    /// Store and product id.
    reference: CatalogRef,
    /// Where it comes from.
    source: Source,
}

/// The catalog sync of one link.
pub struct CatalogSync<S> {
    /// This install's library.
    pub library: LibraryStore,
    /// The library (owner cluster) this install is a branch of.
    pub cluster_id: String,
    /// The link's client.
    pub handle: HostClientHandle,
    /// This install's files.
    pub shelf: S,
}

/// Text for a store id: the book's plugin alias, lowercased.
fn store_id(source: &str) -> String {
    source.trim().to_ascii_lowercase()
}

/// A MIME type from a key's extension.
fn content_type(key: &str) -> Option<String> {
    let ext = key.rsplit_once('.')?.1.to_ascii_lowercase();
    Some(
        match ext.as_str() {
            "m4b" | "m4a" | "mp4" => "audio/mp4",
            "mp3" => "audio/mpeg",
            "pdf" => "application/pdf",
            "jpg" | "jpeg" => "image/jpeg",
            "png" => "image/png",
            "json" => "application/json",
            "cue" | "txt" => "text/plain",
            _ => return None,
        }
        .to_string(),
    )
}

/// The marketplace a shared title names for an own book: its own,
/// lower-cased, or none when it is not a plain marketplace code.
fn own_marketplace(marketplace: &str) -> String {
    let market = marketplace.trim().to_ascii_lowercase();
    if bookclerk_node_proto::is_marketplace(&market) {
        market
    } else {
        String::new()
    }
}

/// Metadata of an own book.
fn own_metadata(book: &BookRecord) -> CatalogMetadata {
    CatalogMetadata {
        title: book.title.clone(),
        subtitle: book.subtitle.clone(),
        authors: book.authors.clone(),
        narrators: book.narrators.clone(),
        series: book.series.clone(),
        series_index: book.series_index.clone(),
        asin: book.asin.clone(),
        isbn: book.isbn.clone(),
        publisher: book.publisher.clone(),
        length_minutes: book.length_minutes,
        content_kind: Some(book.content_kind.clone()),
        language: book.language.clone(),
        published_at: book.published_at.map(|at| at.to_rfc3339()),
    }
}

/// True when `rest` (a name in the primary's directory) is a file beside
/// the primary with its stem: `stem.<anything>`, so `b10.m4b` is not a
/// sibling of `b1.m4b`.
fn is_sibling(rest: &str, stem: &str) -> bool {
    !rest.contains('/')
        && rest
            .strip_prefix(stem)
            .is_some_and(|after| after.starts_with('.'))
}

/// A destination listing that failed: the title it was for may be fine,
/// so a pass does not treat it as gone ([`CatalogSync::sync_once`]).
#[derive(Debug)]
struct ListingFailed(String);

impl std::fmt::Display for ListingFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "listing {:?}", self.0)
    }
}

/// The files of an own book on `shelf`: the primary file, the PDF, and
/// every file beside the primary named `<its stem>.*` (cover,
/// cue, chapter sidecars). Temp files and metadata sidecars are skipped.
async fn own_files(
    shelf: &dyn StorageBackend,
    primary: &str,
    pdf: Option<&str>,
) -> anyhow::Result<Vec<CatalogFile>> {
    let (dir, name) = primary.rsplit_once('/').unwrap_or(("", primary));
    let stem = name.rsplit_once('.').map_or(name, |(stem, _)| stem);
    let prefix = if dir.is_empty() {
        String::new()
    } else {
        format!("{dir}/")
    };
    let listed = shelf
        .list(&prefix)
        .await
        .with_context(|| ListingFailed(prefix.clone()))?;
    let mut files = BTreeMap::new();
    for object in listed {
        let Some(rest) = object.key.strip_prefix(&prefix) else {
            continue;
        };
        let wanted =
            object.key == primary || Some(object.key.as_str()) == pdf || is_sibling(rest, stem);
        let skipped = rest.ends_with(".bookclerk-meta.json") || rest.contains(".bookclerk-tmp-");
        if wanted && !skipped && is_storage_key(&object.key) {
            files.insert(
                object.key.clone(),
                CatalogFile {
                    content_type: content_type(&object.key),
                    key: object.key,
                    size: object.size,
                },
            );
        }
    }
    anyhow::ensure!(
        files.contains_key(primary),
        "the primary file {primary:?} is not on this install's destination"
    );
    Ok(files.into_values().take(MAX_UPLOAD_FILES).collect())
}

impl<S: BranchShelf> CatalogSync<S> {
    /// Runs passes until `shutdown` turns true: on each welcomed
    /// connection, every [`SYNC_INTERVAL`], and on
    /// [`notify_sharing_changed`].
    pub async fn run(self, mut shutdown: watch::Receiver<bool>) {
        let mut connections = self.handle.connections();
        let mut sharing = SHARING_CHANGED.subscribe();
        loop {
            if *shutdown.borrow() {
                return;
            }
            if self.handle.is_connected() {
                match self.sync_once().await {
                    Ok(report) if report.shared => tracing::info!(
                        cluster_id = %self.cluster_id,
                        sent = report.sent,
                        upserted = report.upserted,
                        skipped = report.skipped,
                        delivered = report.delivered,
                        "shared this branch's books with the library"
                    ),
                    Ok(_) => {}
                    Err(err) => tracing::warn!(
                        cluster_id = %self.cluster_id,
                        error = %err,
                        "the catalog sync stopped; it resumes on the next pass"
                    ),
                }
            }
            tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_err() {
                        return;
                    }
                }
                changed = connections.changed() => {
                    if changed.is_err() {
                        return;
                    }
                }
                _ = sharing.changed() => {}
                () = tokio::time::sleep(SYNC_INTERVAL) => {}
            }
        }
    }

    /// Every title this install may share now, in sync order, one per
    /// store and product id (an own book wins over a branch's).
    ///
    /// Reads identity columns only, in keyset pages, so the pass's memory
    /// and every read stay small however large the library is.
    async fn entries(
        &self,
        include_branches: bool,
        members_books: bool,
    ) -> anyhow::Result<Vec<Entry>> {
        let mut seen = BTreeSet::new();
        let mut out = Vec::new();
        let mut after = None;
        // The branch owner's own books go up (and, when the library
        // acquires all of this branch's members' books, every book this
        // branch lists to it), never one the library acquired itself and
        // this install imported.
        let owners = OwnerShareFilter::load(&self.library, &self.cluster_id, members_books).await?;
        loop {
            let page = self
                .library
                .acquired_book_keys_page(after, ENTRY_PAGE)
                .await?;
            let full = u64::try_from(page.len()).unwrap_or(u64::MAX) >= ENTRY_PAGE;
            after = page.last().map(|key| key.id);
            for key in page {
                if !owners.admits(&key) {
                    continue;
                }
                let reference = CatalogRef {
                    store: store_id(&key.source),
                    product_id: key.product_id.clone(),
                };
                if !seen.insert(reference.clone()) {
                    continue;
                }
                // A requested book counts as changed when it was last
                // asked for, so one already past the cursor still goes up.
                let changed = owners
                    .requested_at(&key)
                    .map_or(key.updated_at, |asked| asked.max(key.updated_at));
                out.push(Entry {
                    order: format!(
                        "{}|own|{}",
                        changed.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
                        key.uuid
                    ),
                    reference,
                    source: Source::Own(key),
                });
            }
            if !full {
                break;
            }
        }
        if include_branches {
            let mut after: Option<String> = None;
            loop {
                let page = forwardable_branch_title_keys_page(
                    &self.library,
                    after.as_deref(),
                    BRANCH_ENTRY_PAGE,
                )
                .await?;
                let full = u64::try_from(page.len()).unwrap_or(u64::MAX) >= BRANCH_ENTRY_PAGE;
                after = page.last().map(|key| key.id.clone());
                for key in page.into_iter().filter(|key| key.delivered) {
                    let reference = CatalogRef {
                        store: key.store.clone(),
                        product_id: key.product_id.clone(),
                    };
                    if !seen.insert(reference.clone()) {
                        continue;
                    }
                    out.push(Entry {
                        order: format!("{}|branch|{}", key.updated_at, key.id),
                        reference,
                        source: Source::Branch(key),
                    });
                }
                if !full {
                    break;
                }
            }
        }
        out.sort_by(|a, b| a.order.cmp(&b.order));
        Ok(out)
    }

    /// The full row behind `entry`, or `None` when it is no longer
    /// shareable (removed, no longer acquired, detached, or its files no
    /// longer delivered) since [`Self::entries`] listed it.
    ///
    /// # Errors
    ///
    /// Returns the read error.
    async fn load(&self, entry: &Entry) -> anyhow::Result<Option<Loaded>> {
        Ok(match &entry.source {
            Source::Own(key) => self
                .library
                .get_book_by_uuid(&key.uuid)
                .await?
                .filter(|book| {
                    book.acquire_status == AcquireStatus::Acquired && book.storage_key.is_some()
                })
                .map(|book| Loaded::Own(Box::new(book))),
            Source::Branch(key) => forwardable_branch_title(&self.library, &key.id)
                .await?
                .map(|row| Loaded::Branch(Box::new(row))),
        })
    }

    /// The [`CatalogTitle`] of `entry` (its row read as `loaded`), its
    /// files listed on `shelf`.
    async fn title(
        &self,
        shelf: &dyn StorageBackend,
        entry: &Entry,
        loaded: &Loaded,
    ) -> anyhow::Result<CatalogTitle> {
        match loaded {
            Loaded::Own(book) => {
                let primary = book.storage_key.as_deref().unwrap_or_default();
                let pdf = (book.pdf_status == AcquireStatus::Acquired)
                    .then_some(book.pdf_storage_key.as_deref())
                    .flatten();
                let files = own_files(shelf, primary, pdf).await?;
                let pdf_key = pdf
                    .filter(|pdf| files.iter().any(|file| file.key == *pdf))
                    .map(str::to_string);
                Ok(CatalogTitle {
                    store: entry.reference.store.clone(),
                    product_id: entry.reference.product_id.clone(),
                    marketplace: own_marketplace(&book.marketplace),
                    metadata: own_metadata(book),
                    primary_key: primary.to_string(),
                    pdf_key,
                    files,
                })
            }
            // Passed on as the branch shared it.
            Loaded::Branch(row) => Ok(row.title.clone()),
        }
    }

    /// One pass: open the session, send the titles after its cursor, and
    /// deliver the files the library wants.
    ///
    /// # Errors
    ///
    /// Returns an error when the library cannot be reached or refuses, or a
    /// local read fails; the next pass resumes after what was accepted.
    pub async fn sync_once(&self) -> anyhow::Result<SyncReport> {
        let Some(link) = load_cluster_link(&self.library, &self.cluster_id).await? else {
            return Ok(SyncReport::default());
        };
        if !link.is_active() {
            return Ok(SyncReport::default());
        }
        let session = self
            .handle
            .catalog_begin(link.share_books, link.include_branches)
            .await
            .map_err(|err| call_error("catalog.begin", &err))?;
        if let Err(err) =
            set_cluster_link_ancestry(&self.library, &self.cluster_id, &session.ancestry).await
        {
            tracing::warn!(cluster_id = %self.cluster_id, error = %err, "could not record the library's ancestry");
        }
        let mut report = SyncReport {
            shared: link.share_books,
            ..SyncReport::default()
        };
        if !link.share_books {
            return Ok(report);
        }
        let Some(shelf) = self.shelf.primary().await else {
            // Nothing to share files from (and no pass, so a destination
            // that is only briefly missing withdraws nothing).
            tracing::debug!(cluster_id = %self.cluster_id, "no output destination here; nothing to share");
            return Ok(report);
        };
        let entries = self
            .entries(link.include_branches, session.members_books)
            .await?;
        let by_ref: BTreeMap<CatalogRef, &Entry> = entries
            .iter()
            .map(|entry| (entry.reference.clone(), entry))
            .collect();
        // Resume: files recorded earlier that never arrived.
        let mut wanted: Vec<CatalogRef> = session.wanted.clone();
        let from_start = session.cursor.is_empty();
        let pending: Vec<&Entry> = entries
            .iter()
            .filter(|entry| from_start || entry.order > session.cursor)
            .collect();
        let mut batch = Vec::new();
        let mut batch_bytes = 0usize;
        let mut batch_no = 0u64;
        let mut cursor = session.cursor.clone();
        // Set once a title's files could not be listed: the cursor stays
        // before it (the titles after it are still sent) and the pass does
        // not end, so the library withdraws nothing it did not see and the
        // next pass tries that title again. A title that is not shareable
        // as it stands is skipped for good, and withdrawn if shared before.
        let mut unlisted = false;
        for entry in pending {
            // A read failure ends the pass (the next one resumes after what
            // was accepted); a title gone since it was listed is skipped.
            let Some(loaded) = self.load(entry).await? else {
                tracing::debug!(
                    product_id = %entry.reference.product_id,
                    "a title is no longer shareable; skipped"
                );
                if !unlisted {
                    cursor.clone_from(&entry.order);
                }
                continue;
            };
            match self.title(shelf.as_ref(), entry, &loaded).await {
                Ok(title) if title.problem().is_none() => {
                    let size = serde_json::to_string(&title).map_or(0, |text| text.len());
                    if !batch.is_empty()
                        && (batch.len() >= MAX_CATALOG_BATCH_TITLES
                            || batch_bytes + size > MAX_BATCH_BYTES)
                    {
                        batch_no += 1;
                        let sent = std::mem::take(&mut batch);
                        report.sent += sent.len() as u64;
                        let accepted = self
                            .handle
                            .catalog_sync(session.fence, batch_no, &cursor, false, sent)
                            .await
                            .map_err(|err| call_error("catalog.sync", &err))?;
                        report.upserted += accepted.upserted;
                        report.skipped += accepted.skipped;
                        wanted.extend(accepted.wanted);
                        batch_bytes = 0;
                    }
                    batch_bytes += size;
                    batch.push(title);
                }
                Ok(title) => tracing::warn!(
                    product_id = %title.product_id,
                    problem = title.problem().unwrap_or_default(),
                    "a title is not shareable as it stands; skipped"
                ),
                Err(err) if err.downcast_ref::<ListingFailed>().is_some() => {
                    tracing::warn!(
                        product_id = %entry.reference.product_id,
                        error = %err,
                        "a title's files could not be listed; the next pass tries it again"
                    );
                    unlisted = true;
                }
                Err(err) => tracing::warn!(
                    product_id = %entry.reference.product_id,
                    error = %err,
                    "a title is not shareable as it stands; skipped"
                ),
            }
            if !unlisted {
                cursor.clone_from(&entry.order);
            }
        }
        // Every pass ends with a last batch (even an empty one) marked
        // done, carrying the advanced cursor: a pass from the start that
        // was cut off and resumed then still ends, and the library
        // withdraws what it did not name. On a pass that is not one from
        // the start, `done` changes nothing more on the library. A pass
        // that could not list a title is not done: it resumes before it.
        batch_no += 1;
        report.sent += batch.len() as u64;
        let accepted = self
            .handle
            .catalog_sync(session.fence, batch_no, &cursor, !unlisted, batch)
            .await
            .map_err(|err| call_error("catalog.sync", &err))?;
        report.upserted += accepted.upserted;
        report.skipped += accepted.skipped;
        wanted.extend(accepted.wanted);
        let mut delivered = BTreeSet::new();
        for reference in wanted {
            if !delivered.insert(reference.clone()) {
                continue;
            }
            let Some(entry) = by_ref.get(&reference) else {
                continue;
            };
            // One title that cannot be read (its files gone, its row
            // malformed) does not hold up the others.
            // A read failure ends the pass like one in the send loop; the
            // next pass asks for the title again.
            let Some(loaded) = self.load(entry).await? else {
                tracing::debug!(
                    product_id = %reference.product_id,
                    "a wanted title is no longer shareable; not delivered"
                );
                continue;
            };
            let title = self.title(shelf.as_ref(), entry, &loaded).await;
            let title = match title {
                Ok(title) => title,
                Err(err) => {
                    tracing::warn!(
                        cluster_id = %self.cluster_id,
                        product_id = %reference.product_id,
                        error = %err,
                        "a shared title's files could not be read; not delivered"
                    );
                    continue;
                }
            };
            match self.deliver(shelf.as_ref(), &session, &title).await {
                Ok(()) => report.delivered += 1,
                // The fence moved on or the link dropped: the next pass
                // resumes.
                Err(err) if !self.handle.is_connected() => return Err(err),
                // One title refused (a key in use, a file changed): the
                // others still go.
                Err(err) => tracing::warn!(
                    cluster_id = %self.cluster_id,
                    product_id = %reference.product_id,
                    error = %err,
                    "a shared title's files were not delivered"
                ),
            }
        }
        Ok(report)
    }

    /// Streams one title's files to the library: hashes each from the
    /// shelf, opens the upload, streams each file, and commits.
    async fn deliver(
        &self,
        shelf: &dyn StorageBackend,
        session: &CatalogSession,
        title: &CatalogTitle,
    ) -> anyhow::Result<()> {
        if session.destinations.is_empty() {
            return Ok(());
        }
        let mut files = Vec::with_capacity(title.files.len());
        for file in &title.files {
            let (_, mut body) = shelf
                .get_stream(&file.key, None)
                .await
                .with_context(|| format!("reading {:?}", file.key))?;
            let mut hasher = sha2::Sha256::new();
            let mut seen = 0u64;
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                let n = body.read(&mut buf).await?;
                if n == 0 {
                    break;
                }
                seen += n as u64;
                hasher.update(&buf[..n]);
            }
            anyhow::ensure!(
                seen == file.size,
                "{:?} changed while it was shared",
                file.key
            );
            files.push(UploadFile {
                copies: session
                    .destinations
                    .iter()
                    .map(|destination| FileCopy {
                        destination: destination.clone(),
                        key: file.key.clone(),
                    })
                    .collect(),
                size: file.size,
                sha256: hasher
                    .finalize()
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect(),
                content_type: file.content_type.clone(),
            });
        }
        let reference = title.reference();
        let upload = self
            .handle
            .catalog_upload(session.fence, &reference, files)
            .await
            .map_err(|err| call_error("catalog.upload", &err))?;
        for (index, file) in title.files.iter().enumerate() {
            let (_, body) = shelf
                .get_stream(&file.key, None)
                .await
                .with_context(|| format!("reading {:?}", file.key))?;
            self.handle
                .upload_catalog_file(&upload, index, file.size, body)
                .await
                .map_err(|err| call_error("a catalog file", &err))?;
        }
        self.handle
            .commit_catalog_upload(&upload)
            .await
            .map_err(|err| call_error("the catalog commit", &err))?;
        tracing::info!(
            cluster_id = %self.cluster_id,
            product_id = %reference.product_id,
            files = title.files.len(),
            "shared a title's files with the library"
        );
        Ok(())
    }
}

/// An owner-call failure as an error.
fn call_error(what: &str, err: &OwnerCallError) -> anyhow::Error {
    match err {
        OwnerCallError::Lost => anyhow::anyhow!("{what}: the catalog fence moved on"),
        OwnerCallError::Refused(reason) => anyhow::anyhow!("the library refused {what}: {reason}"),
        OwnerCallError::Unavailable => anyhow::anyhow!("the library was unavailable for {what}"),
    }
}

#[cfg(test)]
#[path = "catalog_sync_tests.rs"]
mod tests;
