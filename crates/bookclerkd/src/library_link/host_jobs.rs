//! How a host runs a job a library handed it (H-4 step 9).
//!
//! [`HostJobTransport`] is the [`JobTransport`] behind every host client.
//! It never runs a job in process against this install's library: the
//! job's guest is the owner's plugin version materialized here, spawned
//! jailed under the grant this host's operator consented to (through the
//! mandatory `bookclerk-workerd` front door), opened with the bindings
//! [`super::remote_bindings::host_job_bindings`] built, and given only the
//! store credentials the owner handed with the job. Any failure to open it
//! fails the job; there is no fallback.
//!
//! - **Scan.** The guest's rows come back unwritten
//!   ([`bookclerk_plugin_host::ExternalSource::scan_handed`]) and go to the
//!   owner in fenced `scan.results` batches of at most
//!   [`MAX_SCAN_BATCH_ROWS`] rows and [`MAX_BATCH_BYTES`] (a row larger
//!   than [`MAX_SCAN_ROW_BYTES`] is skipped). The owner validates and
//!   writes them (decision 6). Each account's pass starts from the sync
//!   token the owner handed ([`bookclerk_node_proto::WorkJob::scan_tokens`])
//!   and then reports the ids a delta pass found gone and how it ended
//!   (`scan.removed`, `scan.pass_end`), so the owner keeps the token and
//!   prunes after a complete full pass. The scan completes with `complete`.
//! - **Acquire.** The host asks the owner for the job's titles
//!   (`acquire.plan`, again until no new title comes back), acquires each
//!   into a per-title outbox under this host's files dir with one outbox
//!   destination per owner destination in the title's target set (the
//!   destinations whose filter takes it, as the plan lists them for each
//!   title; a title none takes fails here), each with that destination's own
//!   naming options, so every destination's keys come out as a local
//!   acquire on the owner would name them (the acquire pipeline runs
//!   against a scratch library
//!   opened through this host's database guest, seeded with the owner's
//!   row; media work stays in `bookclerk-media-worker`), copies the files
//!   to this host's own deployed destinations (H-2) when the title is the
//!   branch owner's ([`bookclerk_node_proto::WorkJob::for_node_owner`]) or
//!   the link's "Keep copies of other members' books" is on, and streams them to
//!   the owner (`upload.grant`, one `PUT` per distinct file content, each
//!   listing its key on every destination, then `commit`), which writes
//!   them straight to its destinations. A title is uploaded only once every
//!   one of this host's own destinations has it; a failed copy fails the
//!   title, and so does any failure of its upload after the copies were
//!   written (the grant, a file, the commit, or a cancel), which removes
//!   those copies again. The job completes with its closing upload's commit, never with
//!   `complete`, and the owner accepts that only once every title of the
//!   job is acquired. A host may stage; the owner never does.
//!
//! **Cancellation.** The job's cancel flag (its fence lost, the operator's
//! cancel, or this host's consent revoked) stops every guest call within
//! [`CANCEL_POLL`]: the call is dropped, which shuts its guest down and
//! fences its host-side authority, so the guest stops using the handed
//! credentials. A title's upload is dropped the same way, and the owner
//! discards what that upload had written.
//!
//! The guest work itself sits behind [`HostGuests`], so tests can stand in
//! a guest without spawning one; [`GuestHost`] is the daemon's.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use bookclerk_config::DestinationId;
use bookclerk_library::control_plane::{self, LinkImportOutcome};
use bookclerk_library::{AcquireTitle, JobKind, LibraryStore, NewBook};
use bookclerk_node_client::{HostJobCtx, LocalFileSource, OwnerCallError};
use bookclerk_node_proto::{
    target_destinations_problem, AcquireTarget, FileCopy, PlanDestination, TargetTitle, UploadFile,
    UploadProbe, MAX_SCAN_BATCH_ROWS, MAX_SCAN_REMOVED_IDS, MAX_SCAN_ROW_BYTES, MAX_UPLOAD_FILES,
};
use bookclerk_plugin_host::{HandedScan, RefreshedCredentialSink};
use bookclerk_source::DownloadOptions;
use bookclerk_storage::{ObjectMeta, StorageBackend};
use sha2::Digest as _;
use tokio::io::AsyncReadExt as _;

use super::remote_bindings::HostJobBindings;
use crate::api::AppState;
use crate::job_handler::{JobCommand, JobExecCtx, JobTransport};

/// Largest serialized `scan.results` batch (well under a frame).
pub const MAX_BATCH_BYTES: usize = 768 * 1024;

/// Attempts for one owner call that found the owner unavailable.
const OWNER_ATTEMPTS: u32 = 3;

/// How often a running guest call or upload checks the job's cancel flag.
pub const CANCEL_POLL: Duration = Duration::from_millis(50);

/// Runs `work` until it ends or `cancel` turns true. On cancel the work is
/// dropped (for the daemon's guests that shuts the guest down) and the
/// answer is a `cancelled` error. Work that ends while `cancel` is set
/// (a guest that stopped cooperatively, returning what it had so far) is
/// answered `cancelled` too, never with its result.
///
/// # Errors
///
/// Returns `work`'s error, or `cancelled`.
pub async fn until_cancelled<T>(
    cancel: &AtomicBool,
    work: impl Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    if cancel.load(Ordering::SeqCst) {
        anyhow::bail!("cancelled");
    }
    tokio::pin!(work);
    let mut every = tokio::time::interval(CANCEL_POLL);
    loop {
        tokio::select! {
            out = &mut work => {
                if cancel.load(Ordering::SeqCst) {
                    anyhow::bail!("cancelled");
                }
                return out;
            }
            _ = every.tick() => {
                if cancel.load(Ordering::SeqCst) {
                    anyhow::bail!("cancelled");
                }
            }
        }
    }
}

/// What a host scan covers.
#[derive(Debug, Clone, Default)]
pub struct HostScan {
    /// Account ids; empty means every handed account.
    pub accounts: Vec<String>,
    /// Import podcast episodes (the owner's setting).
    pub import_episodes: bool,
    /// Import subscription (Plus) titles (the owner's setting).
    pub import_plus_titles: bool,
    /// The owner's sync token per account, from its last complete pass;
    /// an account without one runs a full pass.
    pub sync_tokens: HashMap<String, String>,
}

/// One output destination of the shared node an acquire was planned with.
#[derive(Debug, Clone)]
pub struct HostDestination {
    /// Its id on the shared node (`local`, `s3`, ...).
    pub id: DestinationId,
    /// Its naming and packaging options.
    pub options: DownloadOptions,
}

impl HostDestination {
    /// Parses a plan's destination; refuses an unknown kind or options
    /// this host cannot read.
    ///
    /// # Errors
    ///
    /// Returns an error naming the destination id.
    pub fn from_plan(plan: PlanDestination) -> anyhow::Result<Self> {
        let id = DestinationId::new(plan.id.as_str())
            .map_err(anyhow::Error::msg)
            .with_context(|| format!("the owner's destination {:?}", plan.id))?;
        let options: DownloadOptions = serde_json::from_value(plan.options)
            .with_context(|| format!("the owner's options for destination {:?}", plan.id))?;
        Ok(Self { id, options })
    }
}

/// One title an acquire fetches here.
#[derive(Debug, Clone)]
pub struct HostTitle {
    /// The owner's account holding of the title (`holdings.id` there): the
    /// title this host's upload is granted and committed for.
    pub holding_id: String,
    /// The title as the owner sent it, as a library insert under the
    /// holding id ([`scratch_book`]): what the scratch library is seeded
    /// with and the branch's own library imports.
    pub book: NewBook,
    /// The title's podcast parent as the owner sent it, when it did;
    /// seeded into the scratch library so episodes are named as on the
    /// owner.
    pub parent: Option<NewBook>,
    /// The title's call number on the owner, which names its files here as
    /// there (this host's scratch library has its own edition ids).
    pub call_number: Option<String>,
    /// The podcast parent's call number on the owner.
    pub parent_call_number: Option<String>,
    /// The shared node's destinations in the title's target set
    /// ([`target_set`]), the primary first; never empty.
    pub destinations: Vec<HostDestination>,
}

/// Where destination `index`'s files go inside a title's outbox.
#[must_use]
pub fn destination_outbox(outbox: &Path, index: usize) -> PathBuf {
    outbox.join(format!("d{index}"))
}

/// What acquiring one title left in its outbox.
#[derive(Debug, Clone)]
pub struct AcquiredTitle {
    /// Storage key of the primary audio file on the primary destination.
    pub primary_key: String,
    /// Storage key of the companion PDF on the primary destination, when
    /// one was acquired.
    pub pdf_key: Option<String>,
    /// Every file per destination (in [`HostTitle::destinations`] order),
    /// as storage keys relative to [`destination_outbox`].
    pub keys: Vec<Vec<String>>,
}

/// The guest work a host job does; the daemon's is [`GuestHost`].
///
/// The transport drops a call once the job's cancel flag turns true (see
/// [`until_cancelled`]); `cancel` is the same flag, for guests that can
/// also stop cooperatively.
pub trait HostGuests: Send + Sync + 'static {
    /// Runs the job's guest scan with the handed credentials and answers
    /// its rows unwritten; credentials the guest refreshes go to
    /// `refreshed` ([`OwnerWriteBack`]).
    fn scan(
        &self,
        bindings: &HostJobBindings,
        refreshed: Arc<dyn RefreshedCredentialSink>,
        scan: HostScan,
        cancel: Arc<AtomicBool>,
    ) -> impl Future<Output = anyhow::Result<HandedScan>> + Send;

    /// Acquires `title` with the job's guest into `outbox`: destination
    /// `i`'s files, named with its options, under
    /// [`destination_outbox`]`(outbox, i)`. Credentials the guest
    /// refreshes go to `refreshed` ([`OwnerWriteBack`]).
    fn acquire(
        &self,
        bindings: &HostJobBindings,
        refreshed: Arc<dyn RefreshedCredentialSink>,
        title: &HostTitle,
        outbox: &Path,
        cancel: Arc<AtomicBool>,
    ) -> impl Future<Output = anyhow::Result<AcquiredTitle>> + Send;

    /// This host's own deployed output destinations (H-2: a host's local
    /// outputs add to the library's); empty when none is deployed.
    ///
    /// # Errors
    ///
    /// Returns an error when the deployed destinations cannot be used (two
    /// of one kind), so no title is uploaded while this host would miss
    /// its copy.
    fn local_destinations(
        &self,
    ) -> impl Future<Output = anyhow::Result<Vec<LocalDestination>>> + Send;

    /// Where per-job scratch lives (under this host's files dir).
    fn work_dir(&self) -> PathBuf;

    /// True when the link's "Keep copies of other members' books" is on,
    /// so a title acquired for another member is copied to this host's own
    /// destinations too. False when the setting cannot be read.
    fn keeps_member_copies(&self) -> impl Future<Output = bool> + Send;

    /// Imports a title of the branch owner's that the owner committed into
    /// this install's own library, with the copy this host kept
    /// ([`OwnerTitleCopy`]). Best effort: a failure is logged, never the
    /// job's.
    fn import_owner_title(&self, copy: OwnerTitleCopy<'_>) -> impl Future<Output = ()> + Send;

    /// This install's own copies of the single-keeper accounts it keeps for
    /// the library ([`OwnerWriteBack`]); `None` when it keeps none here.
    fn local_keeper(&self) -> Option<Arc<dyn LocalKeeper>> {
        None
    }

    /// Where this host records the copies it keeps on its own destinations
    /// (#314, [`CopyLedger`]); `None` when it records none, and its copies
    /// then overwrite as a local acquire's would.
    fn copy_ledger(&self) -> impl Future<Output = Option<CopyLedger>> + Send {
        std::future::ready(None)
    }
}

/// Where a host records the copies it keeps on this install's own
/// destinations: this install's own library
/// ([`bookclerk_library::control_plane::claim_link_copy`]), for the library
/// the job is for, by this node.
#[derive(Clone)]
pub struct CopyLedger {
    /// This install's own library.
    pub store: LibraryStore,
    /// The library the job acquires for.
    pub library_id: String,
    /// This node's id, which every attempt's writer starts with.
    pub node_id: String,
}

/// One title's copy attempt in a [`CopyLedger`]: every key it copies is
/// claimed under it first, and the attempt is kept or given back as a
/// whole.
pub struct CopyAttempt {
    /// The ledger.
    ledger: CopyLedger,
    /// The title the keys are claimed for
    /// ([`bookclerk_library::control_plane::link_copy_title_ref`]).
    title_ref: String,
    /// The attempt's writer
    /// ([`bookclerk_library::control_plane::link_copy_writer`]).
    writer: String,
}

impl CopyAttempt {
    /// A new attempt for the title the owner holds as `holding_id`, named
    /// there by `call_number`.
    #[must_use]
    pub fn new(ledger: &CopyLedger, call_number: Option<&str>, holding_id: &str) -> Self {
        Self {
            ledger: ledger.clone(),
            title_ref: control_plane::link_copy_title_ref(call_number, holding_id),
            writer: control_plane::link_copy_writer(
                &ledger.node_id,
                control_plane::current_process_incarnation(),
            ),
        }
    }

    /// Claims `key` on `destination` before it is written (`target` says
    /// whether a file is already there); an error when another title holds
    /// it, so the copy never overwrites that file.
    async fn claim(
        &self,
        destination: &DestinationId,
        key: &str,
        target: control_plane::CopyTarget,
    ) -> anyhow::Result<()> {
        let claim = control_plane::claim_link_copy(
            &self.ledger.store,
            control_plane::LinkCopyAttempt {
                library_id: &self.ledger.library_id,
                title_ref: &self.title_ref,
                writer: &self.writer,
            },
            destination.as_str(),
            key,
            target,
        )
        .await?;
        match claim {
            control_plane::LinkCopyClaim::Fresh | control_plane::LinkCopyClaim::Replaced => Ok(()),
            control_plane::LinkCopyClaim::HeldByAnotherTitle => anyhow::bail!(
                "another title holds {key:?} on this host's own destination {destination}"
            ),
        }
    }

    /// Keeps every copy of the attempt (the owner committed the title, or
    /// its files stay because the outcome is unknown). Logged, not an
    /// error: the files are in place either way.
    async fn keep(&self) {
        if let Err(err) = control_plane::keep_link_copies(&self.ledger.store, &self.writer).await {
            tracing::warn!(writer = %self.writer, error = %err, "could not record a title's copies on this host's own destinations as kept");
        }
    }

    /// Gives back every key of the attempt once its files were removed.
    /// Logged, not an error.
    async fn release(&self) {
        if let Err(err) = control_plane::release_link_copies(&self.ledger.store, &self.writer).await
        {
            tracing::warn!(writer = %self.writer, error = %err, "could not give back a failed title's keys on this host's own destinations");
        }
    }
}

/// Removes the copies an earlier process of this node left mid-title on
/// this host's own destinations ([`control_plane::stale_link_copies`]):
/// their attempt never learned whether the owner committed them, so no one
/// else would ever remove them. A copy written over a file already there
/// (a kept copy of its title, say) keeps its file and is recorded kept
/// again, as the attempt's own cleanup would have left it. Each key is
/// settled on its own, so one whose file could not be deleted stays
/// recorded for the next job's sweep. Best effort, logged.
async fn sweep_stale_copies<G: HostGuests>(guests: &G, ledger: &CopyLedger) {
    let stale = match control_plane::stale_link_copies(
        &ledger.store,
        &ledger.node_id,
        control_plane::current_process_incarnation(),
        chrono::Utc::now(),
    )
    .await
    {
        Ok(stale) if stale.is_empty() => return,
        Ok(stale) => stale,
        Err(err) => {
            tracing::warn!(error = %err, "could not read the unfinished copies on this host's own destinations");
            return;
        }
    };
    let local = match guests.local_destinations().await {
        Ok(local) => local,
        Err(err) => {
            tracing::warn!(error = %err, "could not open this host's own destinations to remove unfinished copies");
            return;
        }
    };
    for copy in &stale {
        match copy.target {
            control_plane::CopyTarget::Existing => {}
            control_plane::CopyTarget::New => {
                match local.iter().find(|(id, _)| id.as_str() == copy.destination) {
                    Some((_, backend)) => match backend.delete(&copy.key).await {
                        Ok(()) | Err(bookclerk_storage::StorageError::NotFound(_)) => {}
                        Err(err) => {
                            tracing::warn!(key = %copy.key, error = %err, "could not remove an unfinished copy from this host's own destination; retried on the next job");
                            continue;
                        }
                    },
                    None => {
                        tracing::warn!(destination = %copy.destination, key = %copy.key, "an unfinished copy is on a destination this host no longer has; left in place")
                    }
                }
            }
        }
        if let Err(err) = control_plane::settle_stale_link_copy(&ledger.store, copy).await {
            tracing::warn!(writer = %copy.writer, error = %err, "could not settle an unfinished copy's record");
        }
    }
    tracing::info!(
        copies = stale.len(),
        "removed copies an earlier run left unfinished on this host's own destinations"
    );
}

/// One of this host's own destinations: its destination id (`local`,
/// `s3`, ...) and backend.
pub type LocalDestination = (DestinationId, Box<dyn StorageBackend>);

/// A title of the branch owner's that the owner committed, as this host
/// kept it ([`HostGuests::import_owner_title`]).
#[derive(Debug, Clone, Copy)]
pub struct OwnerTitleCopy<'a> {
    /// The owner's account holding (`holdings.id`) of the title.
    pub holding_id: &'a str,
    /// The title as the owner sent it.
    pub book: &'a NewBook,
    /// The acquired files' keys.
    pub acquired: &'a AcquiredTitle,
    /// Every file this host holds on its own destinations:
    /// `(destination id, key)`.
    pub files: &'a [(DestinationId, String)],
}

/// The daemon's [`HostGuests`]: spawns the owner's plugin version
/// materialized here as a jailed guest.
pub struct GuestHost {
    /// Daemon state (config, this host's destinations).
    state: Arc<AppState>,
    /// This host's files dir.
    files_dir: PathBuf,
    /// The joined library's own plugin-state subtree, so its guests never
    /// share a `HOME` with another library's copy of the same plugin or
    /// with this install's own.
    state_scope: bookclerk_plugin_host::StateScope,
    /// The joined library's id, to read the link's settings.
    library_id: String,
}

impl GuestHost {
    /// Guests for the daemon at `state`, whose files dir is `files_dir`,
    /// running the plugins of the library with `library_id`.
    #[must_use]
    pub fn new(state: Arc<AppState>, files_dir: PathBuf, library_id: &str) -> Self {
        Self {
            state,
            files_dir,
            state_scope: bookclerk_plugin_host::StateScope::Link(
                bookclerk_plugin_host::LinkStateDir::for_library(library_id),
            ),
            library_id: library_id.to_owned(),
        }
    }

    /// Spawns the job's guest under the consented grant, with only the
    /// handed credentials, sending credentials it refreshes to `refreshed`.
    /// Fails closed.
    async fn open_source(
        &self,
        bindings: &HostJobBindings,
        refreshed: Arc<dyn RefreshedCredentialSink>,
    ) -> anyhow::Result<bookclerk_plugin_host::ExternalSource> {
        let mut plugin =
            bookclerk_plugin_host::open_installed_plugin(&bindings.root, &bindings.version)
                .context("the owner's plugin is not usable on this host")?;
        // Exactly the grant this host's operator consented to.
        plugin.version_grant = Some(bindings.grant.clone());
        plugin.state_scope = self.state_scope.clone();
        let config = self.state.config.read().await.clone();
        let source = bookclerk_plugin_host::ExternalSource::spawn_prepared(
            &plugin,
            &config,
            bookclerk_plugin_host::SessionServices::default(),
            bindings.open.clone(),
        )
        .await
        .context("the owner's plugin did not start on this host")?;
        Ok(source
            .with_handed_credentials(bindings.credentials.clone())
            .with_refresh_sink(refreshed))
    }
}

/// [`GuestHost`]'s [`LocalKeeper`]: this install's library, for the link
/// to `library_id`.
pub(super) struct DaemonKeeper {
    /// Daemon state, for the current library.
    pub(super) state: Arc<AppState>,
    /// The library the jobs come from.
    pub(super) library_id: String,
}

#[async_trait::async_trait]
impl LocalKeeper for DaemonKeeper {
    async fn keep(
        &self,
        plugin_key: &str,
        account_id: &str,
        sent: &[u8],
        refreshed: &[u8],
    ) -> Option<KeptHere> {
        let library = self.state.library_snapshot().await;
        // A shareable account's copies each refresh on their own: its
        // refresh goes back to the library as before.
        let sharing = crate::node_api::branch::credential_sharing(&self.state, &library).await;
        if sharing.get(plugin_key) != Some(&bookclerk_plugin_host::CredentialSharing::SingleKeeper)
        {
            return None;
        }
        match bookclerk_library::control_plane::keeps_for_link(
            &library,
            plugin_key,
            account_id,
            &self.library_id,
        )
        .await
        {
            Ok(true) => {}
            Ok(false) => return None,
            Err(err) => {
                tracing::warn!(account_id, error = %err, "could not read whether this install keeps an account");
                return None;
            }
        }
        let Ok(sent) = serde_json::from_slice::<serde_json::Value>(sent) else {
            tracing::warn!(
                account_id,
                "a job's handed credentials are not JSON; its refresh is not kept here"
            );
            return Some(KeptHere::Failed);
        };
        let scope = bookclerk_library::SourceScope::new(library, plugin_key, "");
        match scope
            .replace_refreshed_credentials(
                account_id,
                &sent,
                refreshed,
                bookclerk_library::RefreshKeeping::SingleKeeper,
            )
            .await
        {
            Ok(true) => Some(KeptHere::Saved),
            Ok(false) => Some(KeptHere::Superseded),
            Err(err) => {
                tracing::warn!(account_id, error = %err, "could not keep a job's refreshed credentials here");
                Some(KeptHere::Failed)
            }
        }
    }
}

impl HostGuests for GuestHost {
    fn local_keeper(&self) -> Option<Arc<dyn LocalKeeper>> {
        Some(Arc::new(DaemonKeeper {
            state: Arc::clone(&self.state),
            library_id: self.library_id.clone(),
        }))
    }

    async fn copy_ledger(&self) -> Option<CopyLedger> {
        let node_id = match control_plane::load_or_create_node_identity(&self.files_dir) {
            Ok(identity) => identity.node_id,
            Err(err) => {
                tracing::warn!(error = %err, "node identity unavailable; this host's own copies go unrecorded");
                return None;
            }
        };
        Some(CopyLedger {
            store: self.state.library_snapshot().await,
            library_id: self.library_id.clone(),
            node_id,
        })
    }

    async fn scan(
        &self,
        bindings: &HostJobBindings,
        refreshed: Arc<dyn RefreshedCredentialSink>,
        scan: HostScan,
        cancel: Arc<AtomicBool>,
    ) -> anyhow::Result<HandedScan> {
        let source = self.open_source(bindings, refreshed).await?;
        // The owner's import settings, not this host's: the owner writes
        // every row into its library.
        Ok(source
            .scan_handed(
                bookclerk_source::ScanOptions {
                    accounts: scan.accounts,
                    page_size: 50,
                    import_episodes: scan.import_episodes,
                    import_plus_titles: scan.import_plus_titles,
                    cancel: Some(cancel),
                },
                &scan.sync_tokens,
            )
            .await?)
    }

    async fn acquire(
        &self,
        bindings: &HostJobBindings,
        refreshed: Arc<dyn RefreshedCredentialSink>,
        title: &HostTitle,
        outbox: &Path,
        _cancel: Arc<AtomicBool>,
    ) -> anyhow::Result<AcquiredTitle> {
        // The pipeline has no cancel input; the transport drops this call
        // (and with it the guest) once the job is cancelled, and all of the
        // title's scratch (its library, and the pipeline's download and
        // packaging dirs) sits under the job dir it removes.
        let source = self.open_source(bindings, refreshed).await?;
        let cfg = self.state.config.read().await.clone();
        let scratch_dir = outbox.with_extension("library");
        tokio::fs::create_dir_all(&scratch_dir).await?;
        let scratch = scratch_library(&cfg, &scratch_dir).await?;
        // This host's own metadata providers fill the plain-audio overlay.
        let metadata = crate::covers::acquire_metadata(&self.state).await;
        let result = acquire_into(
            &scratch.store,
            &cfg,
            &source,
            title,
            outbox,
            &title_cache_dir(outbox),
            metadata,
        )
        .await;
        drop(scratch);
        let _ = tokio::fs::remove_dir_all(&scratch_dir).await;
        result
    }

    async fn local_destinations(&self) -> anyhow::Result<Vec<LocalDestination>> {
        let registry = self.state.destinations.read().await;
        Ok(registry
            .destinations()
            .into_iter()
            .map(|dest| (dest.id, dest.backend.clone_box()))
            .collect())
    }

    fn work_dir(&self) -> PathBuf {
        self.files_dir.join("library-work")
    }

    async fn import_owner_title(&self, copy: OwnerTitleCopy<'_>) {
        let library = self.state.library_snapshot().await;
        let files: Vec<(&str, &str)> = copy
            .files
            .iter()
            .map(|(destination, key)| (destination.as_str(), key.as_str()))
            .collect();
        let import = bookclerk_library::control_plane::LinkImport {
            library_id: &self.library_id,
            book: copy.book,
            primary_key: &copy.acquired.primary_key,
            pdf_key: copy.acquired.pdf_key.as_deref(),
            files: &files,
        };
        let owner_holding_id = copy.holding_id;
        match bookclerk_library::control_plane::import_link_title(&library, import).await {
            Ok(LinkImportOutcome::Imported(title)) => tracing::info!(
                library_id = %self.library_id,
                owner_holding_id,
                holding_id = %title.holding_id,
                "imported the owner's title into this library"
            ),
            Ok(LinkImportOutcome::AccountNotHere) => tracing::info!(
                library_id = %self.library_id,
                owner_holding_id,
                "the owner's title stays out of this library: its store account is not connected here to this install's Owner"
            ),
            Ok(LinkImportOutcome::KeyHeldElsewhere) => tracing::warn!(
                library_id = %self.library_id,
                owner_holding_id,
                "the owner's title stays unacquired in this library: another edition here holds its file key"
            ),
            Ok(LinkImportOutcome::NoPerson) => tracing::info!(
                library_id = %self.library_id,
                owner_holding_id,
                "the owner's title stays out of this library: this install has no single active Owner"
            ),
            Err(err) => tracing::warn!(
                library_id = %self.library_id,
                owner_holding_id,
                error = %err,
                "could not import the owner's title into this library"
            ),
        }
    }

    async fn keeps_member_copies(&self) -> bool {
        let library = self.state.library_snapshot().await;
        match bookclerk_library::control_plane::load_library_link(&library, &self.library_id).await
        {
            Ok(link) => link.is_some_and(|link| link.keep_member_copies),
            Err(err) => {
                tracing::warn!(library_id = %self.library_id, error = %err, "could not read the link's copy setting; keeping no copy of another member's title");
                false
            }
        }
    }
}

/// A scratch library for one title: its own database guest on a file in
/// the title's scratch dir, so the acquire pipeline never touches this
/// install's library.
struct Scratch {
    /// The store; dropped before the guest.
    store: LibraryStore,
    /// Keeps the database guest alive.
    _registry: bookclerk_plugin_host::DatabaseRegistry,
}

/// Opens [`Scratch`] under `dir` through the `sqlite` database guest.
async fn scratch_library(cfg: &bookclerk_config::Config, dir: &Path) -> anyhow::Result<Scratch> {
    let mut scratch = cfg.clone();
    scratch.database.plugin = "sqlite".into();
    scratch.database.sqlite.path = Some(dir.join("scratch.db"));
    let registry = bookclerk_plugin_host::load_external_database(&scratch)
        .await
        .map_err(|err| anyhow::anyhow!("the scratch database guest did not start: {err}"))?;
    let store = bookclerk_plugin_host::open_library_store(&scratch, &registry).await?;
    Ok(Scratch {
        store,
        _registry: registry,
    })
}

/// The title an acquire target names as a library insert: the target's
/// store and account, and the owner's metadata. A date that does not parse
/// as RFC 3339 is left out; it only names files.
#[must_use]
pub fn scratch_book(plugin_key: &str, account_id: &str, title: &TargetTitle) -> NewBook {
    let at = |text: &Option<String>| {
        text.as_deref()
            .and_then(|text| chrono::DateTime::parse_from_rfc3339(text).ok())
            .map(|at| at.with_timezone(&chrono::Utc))
    };
    NewBook {
        product_id: title.product_id.clone(),
        plugin_key: plugin_key.to_string(),
        source: title.source.clone(),
        account_id: account_id.to_string(),
        asin: title.asin.clone(),
        isbn: title.isbn.clone(),
        marketplace: title.marketplace.clone(),
        title: title.title.clone(),
        authors: title.authors.clone(),
        narrators: title.narrators.clone(),
        series: title.series.clone(),
        series_index: title.series_index.clone(),
        series_asin: title.series_product_id.clone(),
        purchased_at: at(&title.purchased_at),
        publisher: title.publisher.clone(),
        length_minutes: title.length_minutes,
        is_abridged: title.is_abridged,
        content_kind: title.content_kind.clone(),
        categories: title.categories.clone(),
        subtitle: title.subtitle.clone(),
        published_at: at(&title.published_at),
    }
}

/// The fields of a title an acquire request names it with, read from a
/// holding ([`AcquireTitle`]): the owner's own, or on a host its scratch
/// library's, so both name the title the same way.
#[derive(Debug, Clone, Copy)]
pub struct RequestTitle<'a> {
    /// Store product id the guest downloads.
    pub product_id: &'a str,
    /// The account holding the pipeline records the acquire under, when
    /// known; without one it is recorded by product id and account.
    pub holding_id: Option<&'a str>,
    /// Display alias of the store plugin (`audible`, `libro`, ...).
    pub source: &'a str,
    /// Store account that holds the title.
    pub account_id: &'a str,
    /// Display title.
    pub title: &'a str,
    /// Author names, comma-separated.
    pub authors: Option<&'a str>,
    /// Narrator names, comma-separated.
    pub narrators: Option<&'a str>,
    /// Series name.
    pub series: Option<&'a str>,
    /// Position in the series, as the store writes it.
    pub series_index: Option<&'a str>,
}

impl<'a> From<&'a AcquireTitle> for RequestTitle<'a> {
    fn from(title: &'a AcquireTitle) -> Self {
        Self {
            product_id: &title.product_id,
            holding_id: Some(title.holding_id.as_str()),
            source: &title.source,
            account_id: &title.account_id,
            title: &title.title,
            authors: title.authors.as_deref(),
            narrators: title.narrators.as_deref(),
            series: title.series.as_deref(),
            series_index: title.series_index.as_deref(),
        }
    }
}

/// The acquire request for `title` named with `options`: what a host's
/// acquire runs, and what the owner computes the title's key scope from
/// (so both name the title the same way).
#[must_use]
pub fn acquire_request(
    cfg: &bookclerk_config::Config,
    title: RequestTitle<'_>,
    options: &DownloadOptions,
) -> bookclerk_acquire::AcquireRequest {
    bookclerk_acquire::AcquireRequest {
        asin: title.product_id.to_string(),
        holding_id: title.holding_id.map(str::to_string),
        source: title.source.to_string(),
        account_id: title.account_id.to_string(),
        title: title.title.to_string(),
        authors: title.authors.map(str::to_string),
        narrators: title.narrators.map(str::to_string),
        series: title.series.map(str::to_string),
        series_index: title.series_index.map(str::to_string),
        options: options.clone(),
        files_dir: cfg.paths().files_dir.clone(),
        cache_dir: cfg.download_cache_dir(),
        force: false,
        write_destinations: None,
        placement: None,
        job_id: None,
        temp_quota_bytes: Some(cfg.jobs.temp_quota_bytes),
        call_number: None,
        parent_call_number: None,
        metadata: bookclerk_acquire::AcquireMetadata::default(),
        library_copies: bookclerk_acquire::LibraryCopySource::default(),
        destination_keys: bookclerk_acquire::DestinationKeys::default(),
    }
}

/// Where the acquire pipeline downloads and packages a title fetched into
/// `outbox`: beside its scratch library, inside the job dir, so a dropped
/// acquire (a cancel, a lost fence) leaves nothing once the job dir is
/// removed, and no other run (this host's own jobs, or a retry of the
/// title) shares it. Only this daemon touches it; the guest fetches into
/// its own plugin `tmp`.
#[must_use]
pub fn title_cache_dir(outbox: &Path) -> PathBuf {
    outbox.with_extension("cache")
}

/// Runs the acquire pipeline for `title` against `scratch`, writing only
/// to `outbox`, with its scratch under `cache_dir`, asking `metadata` for the
/// plain-audio overlay.
async fn acquire_into(
    scratch: &LibraryStore,
    cfg: &bookclerk_config::Config,
    source: &bookclerk_plugin_host::ExternalSource,
    title: &HostTitle,
    outbox: &Path,
    cache_dir: &Path,
    metadata: bookclerk_acquire::AcquireMetadata,
) -> anyhow::Result<AcquiredTitle> {
    scratch
        .scope(title.book.plugin_key.as_str(), title.book.source.as_str())
        .ensure_account(&title.book.account_id, &title.book.marketplace, None)
        .await?;
    if let Some(parent) = &title.parent {
        scratch.upsert_book(parent).await?;
    }
    let book = &scratch.upsert_book(&title.book).await?;
    let Some(primary) = title.destinations.first() else {
        anyhow::bail!("the owner planned no destination");
    };
    let mut items = Vec::with_capacity(title.destinations.len());
    for (index, destination) in title.destinations.iter().enumerate() {
        let dir = destination_outbox(outbox, index);
        tokio::fs::create_dir_all(&dir).await?;
        items.push(bookclerk_acquire::AcquireDestination {
            id: destination.id.clone(),
            // The owner's destinations translate the keys when the files
            // reach them, so the outbox keeps host keys.
            backend: Box::new(bookclerk_storage::LocalFsBackend::new(dir)?.keeping_host_keys()),
            options: destination.options.clone(),
            // The owner chose these destinations for the title.
            filter: bookclerk_config::DestinationFilter::default(),
        });
    }
    // The pipeline packages once and writes each destination under the
    // keys its own options give, exactly as a local acquire does.
    let destinations = bookclerk_acquire::AcquireDestinations {
        items,
        primary: primary.id.clone(),
        multi_destination: cfg.output.multi_destination,
    };
    // The owner names keys from the same request; the scratch dir does not
    // enter a key.
    let mut request = acquire_request(cfg, book.into(), &primary.options);
    request.cache_dir = cache_dir.to_path_buf();
    request.call_number.clone_from(&title.call_number);
    request
        .parent_call_number
        .clone_from(&title.parent_call_number);
    request.metadata = metadata;
    let result =
        bookclerk_acquire::acquire_book_indexed(scratch, &destinations, request, None, source)
            .await?;
    let pdf_key = scratch
        .acquire_title_of_holding(&book.holding_id)
        .await?
        .filter(|row| row.pdf_status == bookclerk_library::AcquireStatus::Acquired)
        .and_then(|row| row.pdf_storage_key);
    let mut keys = Vec::with_capacity(title.destinations.len());
    for index in 0..title.destinations.len() {
        keys.push(outbox_keys(&destination_outbox(outbox, index)).await?);
    }
    Ok(AcquiredTitle {
        primary_key: result.storage_key,
        pdf_key,
        keys,
    })
}

/// Every finished file under `dir`, as storage keys relative to it
/// (sorted; in-flight temp files are skipped).
///
/// # Errors
///
/// Returns an error when the directory cannot be read.
pub async fn outbox_keys(dir: &Path) -> anyhow::Result<Vec<String>> {
    let mut keys = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(next) = stack.pop() {
        let mut entries = match tokio::fs::read_dir(&next).await {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => return Err(err.into()),
        };
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            let kind = entry.file_type().await?;
            if kind.is_dir() {
                stack.push(path);
                continue;
            }
            if !kind.is_file() {
                continue;
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            // In-flight temps, and the outbox backend's own metadata
            // sidecars (each destination writes its own on receipt).
            if (name.starts_with('.') && name.contains(".bookclerk-tmp-"))
                || name.ends_with(".bookclerk-meta.json")
            {
                continue;
            }
            let Ok(relative) = path.strip_prefix(dir) else {
                continue;
            };
            let key = relative
                .components()
                .map(|part| part.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            keys.push(key);
        }
    }
    keys.sort();
    Ok(keys)
}

/// The [`JobTransport`] for jobs a library handed this host.
pub struct HostJobTransport<G> {
    /// Guest work.
    guests: G,
}

impl<G: HostGuests> HostJobTransport<G> {
    /// A transport over `guests`.
    pub fn new(guests: G) -> Self {
        Self { guests }
    }
}

impl<G: HostGuests> JobTransport for HostJobTransport<G> {
    async fn execute(&self, cmd: JobCommand, ctx: JobExecCtx) -> anyhow::Result<String> {
        let (Some(host), Some(bindings)) = (ctx.host.clone(), ctx.host_bindings.clone()) else {
            anyhow::bail!("only jobs a library handed this host run here");
        };
        if ctx.is_cancelled() {
            anyhow::bail!("cancelled");
        }
        match cmd.kind {
            JobKind::Scan => {
                // The owner writes every row, so the scan imports what the
                // owner's library does; an owner that did not say is
                // refused rather than scanned with this host's settings.
                let Some(import) = host.job.job.scan_import else {
                    anyhow::bail!("the owner sent no import settings for the scan");
                };
                let scan = HostScan {
                    accounts: cmd.payload.account.clone().into_iter().collect(),
                    import_episodes: import.import_episodes,
                    import_plus_titles: import.import_plus_titles,
                    sync_tokens: host
                        .job
                        .job
                        .scan_tokens
                        .iter()
                        .map(|t| (t.account_id.clone(), t.sync_token.clone()))
                        .collect(),
                };
                host_scan(&self.guests, &host, &bindings, scan, &ctx).await
            }
            JobKind::Acquire => host_acquire(&self.guests, &host, &bindings, &ctx).await,
            other => anyhow::bail!("{} jobs do not run on a host", other.as_str()),
        }
    }
}

/// Writes credentials a guest refreshed on this host back to the owner,
/// under the job's fence ([`HostJobCtx::write_back`]), as the row the owner
/// handed for the account. A failed write-back is logged; the job goes on
/// with the refreshed credentials either way: one sink serves the whole
/// job, so its later guests are sent them ([`RefreshedCredentialSink::latest`]).
///
/// When this install keeps the account for the library itself (a
/// single-keeper account its person brought here; [`LocalKeeper`]), the
/// refresh is saved here instead and reaches the library as this branch's
/// next push: the library refuses write-backs of accounts a branch keeps.
///
/// Otherwise the owner replaces the account's row under the job's fence
/// without comparing it with what it handed, so a sign-in on the owner
/// during the job can be overwritten.
pub struct OwnerWriteBack {
    /// The job, whose fence and handed secrets the write-back uses.
    host: Arc<HostJobCtx>,
    /// The job's plugin key, the provider of the account rows it may write.
    plugin_key: String,
    /// The newest refreshed credentials of each account this job: what its
    /// guests hold, and what later guests are sent.
    latest: std::sync::Mutex<BTreeMap<String, zeroize::Zeroizing<Vec<u8>>>>,
    /// For an account this install keeps, the copy this job last saved (or
    /// found newer) here: what the next local save is compared against. A
    /// failed save leaves it as it was, so the next refresh is saved over
    /// the copy that is really stored.
    saved: std::sync::Mutex<BTreeMap<String, zeroize::Zeroizing<Vec<u8>>>>,
    /// Accounts whose newest refresh this install keeps but could not save
    /// yet: saved by the next refresh, or by [`OwnerWriteBack::flush`] when
    /// the job ends.
    pending: std::sync::Mutex<std::collections::BTreeSet<String>>,
    /// This install's own copies of the accounts it keeps for the library.
    local: Option<Arc<dyn LocalKeeper>>,
}

/// What became of a job's refresh of an account this install keeps
/// ([`LocalKeeper::keep`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeptHere {
    /// Saved as this install's copy, for its next push.
    Saved,
    /// Not saved: a newer sign-in here won.
    Superseded,
    /// Not saved: the write failed, so this install's copy is still the
    /// one the job was handed. The job's next refresh is compared with that
    /// one again, and the save is retried when the job ends.
    Failed,
}

/// This install's own copy of a single-keeper account it keeps for the
/// library a job came from ([`OwnerWriteBack`]).
#[async_trait::async_trait]
pub trait LocalKeeper: Send + Sync {
    /// Saves `refreshed` as this install's copy of the account while that
    /// copy is still `sent` (the credentials the guest was given).
    ///
    /// # Returns
    ///
    /// `None` when this install does not keep the account for the library
    /// (the write-back goes to the owner); otherwise what became of the
    /// refresh here (it never goes to the owner then).
    async fn keep(
        &self,
        plugin_key: &str,
        account_id: &str,
        sent: &[u8],
        refreshed: &[u8],
    ) -> Option<KeptHere>;
}

impl OwnerWriteBack {
    /// The sink for the job `host` running `bindings`' plugin.
    #[must_use]
    pub fn sink(
        host: &Arc<HostJobCtx>,
        bindings: &HostJobBindings,
        local: Option<Arc<dyn LocalKeeper>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            host: Arc::clone(host),
            plugin_key: bindings.plugin_key.clone(),
            latest: std::sync::Mutex::default(),
            saved: std::sync::Mutex::default(),
            pending: std::sync::Mutex::default(),
            local,
        })
    }

    /// Saves, when the job ends, each refresh this install keeps whose save
    /// failed and that no later refresh saved: the guests are gone, so this
    /// is the last chance to keep their newest credentials.
    pub async fn flush(&self) {
        let pending: Vec<String> =
            std::mem::take(&mut *self.pending.lock().unwrap_or_else(|p| p.into_inner()))
                .into_iter()
                .collect();
        for account_id in pending {
            let latest = self
                .latest
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .get(&account_id)
                .cloned();
            let (Some(credentials), Some(local)) = (latest, &self.local) else {
                continue;
            };
            let sent = self.stored_here(&account_id);
            match local
                .keep(&self.plugin_key, &account_id, &sent, &credentials)
                .await
            {
                Some(KeptHere::Saved) => tracing::info!(
                    account_id,
                    "refreshed store credentials kept here when the job ended"
                ),
                Some(KeptHere::Superseded) => tracing::info!(
                    account_id,
                    "a newer sign-in here won over a job's refreshed credentials"
                ),
                Some(KeptHere::Failed) => tracing::warn!(
                    account_id,
                    "could not keep a job's refreshed credentials here; this install's copy stays the older one"
                ),
                None => {}
            }
        }
    }

    /// What this install holds of `account_id`: the copy this job last
    /// saved here, else the handed copy.
    fn stored_here(&self, account_id: &str) -> zeroize::Zeroizing<Vec<u8>> {
        let secret_ref = self.host.secrets.refs().find(|secret| {
            secret.kind == bookclerk_library::secrets::secret_kind::SOURCE_AUTH
                && secret.provider == self.plugin_key
                && secret.account_id == account_id
        });
        self.saved
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(account_id)
            .cloned()
            .or_else(|| {
                secret_ref.and_then(|secret_ref| {
                    self.host
                        .secrets
                        .get(secret_ref)
                        .map(|value| zeroize::Zeroizing::new(value.expose().to_vec()))
                })
            })
            .unwrap_or_default()
    }
}

#[async_trait::async_trait]
impl RefreshedCredentialSink for OwnerWriteBack {
    fn latest(&self, account_id: &str) -> Option<Vec<u8>> {
        let latest = self.latest.lock().unwrap_or_else(|p| p.into_inner());
        latest.get(account_id).map(|value| value.to_vec())
    }

    async fn refreshed(&self, account_id: &str, credentials: Vec<u8>) {
        self.refresh(account_id, credentials).await;
    }
}

impl OwnerWriteBack {
    /// Records `credentials` in `map` for `account_id`.
    fn remember(
        map: &std::sync::Mutex<BTreeMap<String, zeroize::Zeroizing<Vec<u8>>>>,
        account_id: &str,
        credentials: &zeroize::Zeroizing<Vec<u8>>,
    ) {
        map.lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(account_id.to_owned(), credentials.clone());
    }

    /// Keeps a refresh here or writes it back to the owner
    /// ([`RefreshedCredentialSink::refreshed`]).
    async fn refresh(&self, account_id: &str, credentials: Vec<u8>) {
        let credentials = zeroize::Zeroizing::new(credentials);
        let Some(secret_ref) = self
            .host
            .secrets
            .refs()
            .find(|secret| {
                secret.kind == bookclerk_library::secrets::secret_kind::SOURCE_AUTH
                    && secret.provider == self.plugin_key
                    && secret.account_id == account_id
            })
            .cloned()
        else {
            tracing::warn!(
                account_id,
                "a guest refreshed credentials the owner did not hand; dropped"
            );
            return;
        };
        // The guest holds these now, whatever becomes of them here.
        Self::remember(&self.latest, account_id, &credentials);
        let sent = self.stored_here(account_id);
        if let Some(local) = &self.local {
            // Pending until the save settles, so a job cancelled while it
            // is in flight still retries it at its end.
            self.pending
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(account_id.to_owned());
            let mut kept = local
                .keep(&self.plugin_key, account_id, &sent, &credentials)
                .await;
            // One retry of a failed save; after that the next refresh, or
            // the job's end, tries again against the copy really stored.
            if kept == Some(KeptHere::Failed) {
                kept = local
                    .keep(&self.plugin_key, account_id, &sent, &credentials)
                    .await;
            }
            if kept != Some(KeptHere::Failed) {
                self.pending
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .remove(account_id);
            }
            match kept {
                Some(KeptHere::Saved) => {
                    Self::remember(&self.saved, account_id, &credentials);
                    tracing::info!(
                        account_id,
                        "refreshed store credentials kept here, for this branch's next push"
                    );
                    return;
                }
                Some(KeptHere::Superseded) => {
                    tracing::info!(
                        account_id,
                        "a newer sign-in here won over a job's refreshed credentials"
                    );
                    return;
                }
                // The copy here is still the one compared against, so the
                // job's next refresh (or its end) saves over it; its guests
                // already have the new credentials.
                Some(KeptHere::Failed) => {
                    tracing::warn!(
                        account_id,
                        "could not keep a job's refreshed credentials here yet; retried when the job ends"
                    );
                    return;
                }
                None => {}
            }
        }
        match self.host.write_back(&secret_ref, &credentials).await {
            Ok(()) => tracing::info!(
                account_id,
                "refreshed store credentials written back to the owner"
            ),
            Err(err) => tracing::warn!(
                account_id,
                error = %err,
                "could not write refreshed store credentials back to the owner"
            ),
        }
    }
}

/// Retries `call` while the owner is unavailable.
async fn with_retries<T, F, Fut>(mut call: F) -> Result<T, OwnerCallError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, OwnerCallError>>,
{
    let mut attempt = 0;
    loop {
        attempt += 1;
        match call().await {
            Err(OwnerCallError::Unavailable) if attempt < OWNER_ATTEMPTS => {
                tokio::time::sleep(Duration::from_millis(500 * u64::from(attempt))).await;
            }
            other => return other,
        }
    }
}

/// An owner-call failure as the job's error.
fn owner_error(what: &str, err: &OwnerCallError) -> anyhow::Error {
    match err {
        OwnerCallError::Lost => anyhow::anyhow!("cancelled: the job's fence was lost ({what})"),
        OwnerCallError::Refused(reason) => anyhow::anyhow!("the owner refused {what}: {reason}"),
        OwnerCallError::Unavailable => anyhow::anyhow!("the owner was unavailable for {what}"),
    }
}

/// A host scan: the guest's rows go back in fenced batches.
async fn host_scan<G: HostGuests>(
    guests: &G,
    host: &Arc<HostJobCtx>,
    bindings: &HostJobBindings,
    scan: HostScan,
    ctx: &JobExecCtx,
) -> anyhow::Result<String> {
    let refreshed = OwnerWriteBack::sink(host, bindings, guests.local_keeper());
    let rows = until_cancelled(
        &ctx.cancel,
        guests.scan(
            bindings,
            Arc::clone(&refreshed) as Arc<dyn RefreshedCredentialSink>,
            scan,
            Arc::clone(&ctx.cancel),
        ),
    )
    .await;
    // The guest is done refreshing, whether or not the scan finished.
    refreshed.flush().await;
    let HandedScan { rows, passes } = rows?;
    let mut batch = Vec::new();
    let mut batch_bytes = 0usize;
    let mut written = 0u64;
    let mut skipped = 0u64;
    let total = rows.len();
    let send = |batch: Vec<serde_json::Value>| async move {
        with_retries(|| host.send_scan_results(&bindings.plugin_key, batch.clone()))
            .await
            .map_err(|err| owner_error("the scan results", &err))
    };
    for row in rows {
        if ctx.is_cancelled() {
            anyhow::bail!("cancelled");
        }
        let value = serde_json::to_value(&row)?;
        let size = value.to_string().len();
        if size > MAX_SCAN_ROW_BYTES {
            skipped += 1;
            tracing::warn!(job_id = %host.job.job.id, size, "skipping a scan row too large to return");
            continue;
        }
        if batch.len() >= MAX_SCAN_BATCH_ROWS || batch_bytes + size > MAX_BATCH_BYTES {
            written += send(std::mem::take(&mut batch)).await?;
            batch_bytes = 0;
        }
        batch_bytes += size;
        batch.push(value);
    }
    if !batch.is_empty() {
        written += send(batch).await?;
    }
    // Then each account's pass: the ids a delta pass found gone, and how it
    // ended. When a row was skipped (too large to send), no pass reports
    // itself complete, which would remove the skipped title from the
    // owner's library, nor hands on its token, which would leave the
    // owner's copy of it stale until a full pass: the next pass is full.
    let mut removed = 0u64;
    for pass in passes {
        for ids in pass.removed.chunks(MAX_SCAN_REMOVED_IDS) {
            if ctx.is_cancelled() {
                anyhow::bail!("cancelled");
            }
            removed += with_retries(|| {
                host.send_scan_removed(&bindings.plugin_key, &pass.account_id, ids.to_vec())
            })
            .await
            .map_err(|err| owner_error("the scan removals", &err))?;
        }
        if ctx.is_cancelled() {
            anyhow::bail!("cancelled");
        }
        let (sync_token, complete) = if skipped == 0 {
            (pass.sync_token.as_str(), pass.complete)
        } else {
            ("", false)
        };
        removed += with_retries(|| {
            host.send_scan_pass_end(
                &bindings.plugin_key,
                &pass.account_id,
                sync_token,
                complete,
                pass.full,
            )
        })
        .await
        .map_err(|err| owner_error("the scan pass end", &err))?;
    }
    // A scan cancelled while its last batch went out did not finish: its
    // rows so far stay written, but it never reports success.
    if ctx.is_cancelled() {
        anyhow::bail!("cancelled");
    }
    tracing::info!(
        job_id = %host.job.job.id,
        fence = host.job.fence,
        rows = total,
        written,
        skipped,
        "host scan returned its rows to the owner"
    );
    Ok(format!(
        "{written} book upsert(s) and {removed} removal(s) from a host scan ({skipped} skipped)"
    ))
}

/// A host acquire: titles from the owner, files streamed back, completed
/// by the closing upload.
async fn host_acquire<G: HostGuests>(
    guests: &G,
    host: &Arc<HostJobCtx>,
    bindings: &HostJobBindings,
    ctx: &JobExecCtx,
) -> anyhow::Result<String> {
    let job_dir = job_work_path(&guests.work_dir(), &host.job.job.id, host.job.fence)?;
    let writer = OwnerWriteBack::sink(host, bindings, guests.local_keeper());
    let refreshed: Arc<dyn RefreshedCredentialSink> = Arc::clone(&writer) as _;
    let result = acquire_titles(guests, host, bindings, &refreshed, ctx, &job_dir).await;
    // The guests are done refreshing, whether or not every title came.
    writer.flush().await;
    let _ = tokio::fs::remove_dir_all(&job_dir).await;
    let (acquired, failed) = result?;
    let detail = format!("acquired={acquired} failed={failed} (on a host)");
    if failed > 0 {
        anyhow::bail!("{detail}");
    }
    if ctx.is_cancelled() {
        anyhow::bail!("cancelled");
    }
    let closing = with_retries(|| host.grant_upload("", "", None, Vec::new()))
        .await
        .map_err(|err| owner_error("the closing upload", &err))?;
    let completed = with_retries(|| host.commit_upload(&closing, Some(&detail)))
        .await
        .map_err(|err| owner_error("the closing commit", &err))?;
    if !completed {
        anyhow::bail!("the owner did not complete the job with its closing upload");
    }
    Ok(detail)
}

/// Acquires and uploads every title the owner hands out; answers how many
/// were acquired and how many failed.
async fn acquire_titles<G: HostGuests>(
    guests: &G,
    host: &Arc<HostJobCtx>,
    bindings: &HostJobBindings,
    refreshed: &Arc<dyn RefreshedCredentialSink>,
    ctx: &JobExecCtx,
    job_dir: &Path,
) -> anyhow::Result<(u32, u32)> {
    let mut attempted = BTreeSet::new();
    let mut acquired = 0u32;
    let mut failed = 0u32;
    // Read once per job, so every title of one job follows one setting.
    let keep_copy = keeps_local_copy(host, guests).await;
    // The copies are recorded in this install's own library, so a copy
    // never overwrites another title's file and one an earlier run left
    // unfinished is removed now (#314).
    let ledger = if keep_copy {
        guests.copy_ledger().await
    } else {
        None
    };
    if let Some(ledger) = &ledger {
        sweep_stale_copies(guests, ledger).await;
    }
    'plan: loop {
        let (targets, planned) = with_retries(|| host.acquire_plan())
            .await
            .map_err(|err| owner_error("the acquire plan", &err))?;
        if planned.is_empty() {
            anyhow::bail!("the owner planned no destination");
        }
        let destinations = planned
            .iter()
            .cloned()
            .map(HostDestination::from_plan)
            .collect::<anyhow::Result<Vec<_>>>()?;
        let fresh: Vec<AcquireTarget> = targets
            .into_iter()
            .filter(|target| !attempted.contains(&target.holding_id))
            .collect();
        if fresh.is_empty() {
            return Ok((acquired, failed));
        }
        let mut fresh = fresh.into_iter();
        while let Some(target) = fresh.next() {
            if ctx.is_cancelled() {
                anyhow::bail!("cancelled");
            }
            attempted.insert(target.holding_id.clone());
            let call_number = target.call_number.clone();
            if target.plugin_key != bindings.plugin_key {
                failed += 1;
                continue;
            }
            // The title goes only to its target set, as the owner chose it
            // by its destinations' filters; nothing takes it, or a list
            // that does not fit the plan, fails it before anything is
            // fetched.
            let targets = match target_set(&target, &planned, &destinations) {
                Ok(targets) => targets,
                Err(err) => {
                    tracing::warn!(job_id = %host.job.job.id, book_uuid = %target.holding_id, error = %err, "a title failed on the host");
                    failed += 1;
                    continue;
                }
            };
            let book = scratch_book(&target.plugin_key, &target.account_id, &target.title);
            let parent = target
                .parent
                .as_ref()
                .map(|parent| scratch_book(&target.plugin_key, &target.account_id, parent));
            let outbox = work_path(job_dir, &target.holding_id)?;
            let title = HostTitle {
                holding_id: target.holding_id,
                book,
                parent,
                call_number: target.call_number.map(String::from),
                parent_call_number: target.parent_call_number.map(String::from),
                destinations: targets,
            };
            // Both the guest's acquire and the title's upload stop once the
            // job is cancelled; a dropped upload's files are discarded by
            // the owner (its stream breaks, or its upload is aborted).
            let copied = std::sync::Mutex::new(Vec::new());
            let attempt = ledger.as_ref().map(|ledger| {
                CopyAttempt::new(ledger, title.call_number.as_deref(), &title.holding_id)
            });
            let commit_unsettled = AtomicBool::new(false);
            let uploaded = until_cancelled(&ctx.cancel, async {
                let done = guests
                    .acquire(
                        bindings,
                        Arc::clone(refreshed),
                        &title,
                        &outbox,
                        Arc::clone(&ctx.cancel),
                    )
                    .await?;
                upload_title(
                    guests,
                    host,
                    &title,
                    &outbox,
                    &done,
                    keep_copy.then_some(KeptCopy {
                        copied: &copied,
                        attempt: attempt.as_ref(),
                    }),
                    &commit_unsettled,
                )
                .await
            })
            .await;
            if uploaded.is_err() {
                let copies = std::mem::take(&mut *copied.lock().unwrap_or_else(|e| e.into_inner()));
                if commit_unsettled.load(Ordering::SeqCst) {
                    // The commit was sent but never definitively answered
                    // (its answer was lost, the owner went away, or the job
                    // was cancelled meanwhile): the owner may have marked
                    // the title acquired, and a later plan would then skip
                    // it, so its copies here stay.
                    if !copies.is_empty() {
                        tracing::warn!(
                            job_id = %host.job.job.id,
                            book_uuid = %title.holding_id,
                            files = copies.len(),
                            "the owner's answer to a title's commit is unknown; keeping its copies on this host's own destinations"
                        );
                    }
                    if let Some(attempt) = &attempt {
                        attempt.keep().await;
                    }
                } else {
                    // The owner never committed the title (its grant or a
                    // file failed, or it refused the commit), so this host
                    // keeps no copy of it either, unless the fence is lost:
                    // a replacement run may own those keys by now.
                    let lost = host.is_lost();
                    remove_local_copies(&title.holding_id, &copies, lost).await;
                    if let Some(attempt) = &attempt {
                        if lost {
                            // The files stay for the replacement run, which
                            // takes over the title's keys.
                            attempt.keep().await;
                        } else {
                            attempt.release().await;
                        }
                    }
                }
            }
            let _ = tokio::fs::remove_dir_all(&outbox).await;
            let _ = tokio::fs::remove_dir_all(title_cache_dir(&outbox)).await;
            match uploaded {
                Ok(()) => {
                    acquired += 1;
                    // Another account's title of the same edition is
                    // planned too: ask again, so the owner shares the files
                    // just uploaded rather than this host fetching and
                    // uploading the same edition a second time.
                    if call_number.is_some()
                        && fresh
                            .as_slice()
                            .iter()
                            .any(|next| next.call_number == call_number)
                    {
                        continue 'plan;
                    }
                }
                Err(err) if ctx.is_cancelled() => return Err(err),
                Err(err) => {
                    tracing::warn!(job_id = %host.job.job.id, book_uuid = %title.holding_id, error = %err, "a title failed on the host");
                    failed += 1;
                }
            }
        }
    }
}

/// The destinations `target` is written and uploaded to: those of the
/// plan its list names ([`AcquireTarget::destinations`]), in the plan's
/// order, or every one when the owner sent no list (an owner that predates
/// destination filters).
///
/// # Arguments
///
/// * `target` - One title of the plan.
/// * `planned` - The plan's destinations as the owner sent them.
/// * `destinations` - The same destinations, parsed
///   ([`HostDestination::from_plan`]), in the same order.
///
/// # Errors
///
/// Returns an error when the list names a destination the plan lacks,
/// repeats or reorders one, or is empty (no destination accepts the
/// title).
pub fn target_set(
    target: &AcquireTarget,
    planned: &[PlanDestination],
    destinations: &[HostDestination],
) -> anyhow::Result<Vec<HostDestination>> {
    if let Some(problem) = target_destinations_problem(target, planned) {
        anyhow::bail!("the owner's destinations for the title do not fit its plan ({problem})");
    }
    let Some(chosen) = target.destinations.as_deref() else {
        return Ok(destinations.to_vec());
    };
    let set: Vec<HostDestination> = destinations
        .iter()
        .filter(|destination| chosen.iter().any(|id| id == destination.id.as_str()))
        .cloned()
        .collect();
    if set.is_empty() {
        anyhow::bail!("no destination accepts this book");
    }
    Ok(set)
}

/// Longest owner-supplied id used as a path segment under the work dir.
pub const MAX_WORK_ID_BYTES: usize = 128;

/// True when `id` (a job or title id the owner sent) is safe as one path
/// segment: non-empty, at most [`MAX_WORK_ID_BYTES`], and only ASCII
/// letters, digits, `-` and `_`, so no separator, `.` / `..`, drive or
/// root prefix, or other special name can come through.
#[must_use]
pub fn is_work_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_WORK_ID_BYTES
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// `dir`'s child named by the owner-supplied `id`, refused unless `id` is
/// one plain segment ([`is_work_id`]), so the host's job scratch (created,
/// written and removed recursively) never leaves `dir`.
///
/// # Errors
///
/// Returns an error when `id` is not a usable segment.
pub fn work_path(dir: &Path, id: &str) -> anyhow::Result<PathBuf> {
    if !is_work_id(id) {
        anyhow::bail!("the owner sent a job or title id this host cannot use as a path");
    }
    Ok(dir.join(id))
}

/// The scratch dir of one fence generation of job `job_id` under `dir`:
/// `<job_id>-g<fence>`, one plain segment. A stale run still unwinding
/// after its lease expired and the replacement generation this host
/// claimed never share it, so the stale run's cleanup removes only its own
/// scratch. The name is unambiguous because `fence` is all digits: it is
/// whatever follows the last `-g`.
///
/// # Errors
///
/// Returns an error when `job_id` is not a usable segment ([`is_work_id`]).
pub fn job_work_path(dir: &Path, job_id: &str, fence: i64) -> anyhow::Result<PathBuf> {
    let job_dir = work_path(dir, job_id)?;
    let mut name = job_dir.into_os_string();
    name.push(format!("-g{fence}"));
    Ok(PathBuf::from(name))
}

/// Size and SHA-256 of a file, read once.
async fn digest_file(path: &Path) -> anyhow::Result<(u64, String)> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = sha2::Sha256::new();
    let mut size = 0u64;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        size += n as u64;
        hasher.update(&buf[..n]);
    }
    Ok((
        size,
        hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect(),
    ))
}

/// MIME type for a storage key.
fn content_type(key: &str) -> Option<String> {
    let ext = key.rsplit('.').next()?.to_ascii_lowercase();
    Some(
        match ext.as_str() {
            "m4b" | "m4a" | "mp4" => "audio/mp4",
            "mp3" => "audio/mpeg",
            "flac" => "audio/flac",
            "ogg" | "oga" | "opus" => "audio/ogg",
            "pdf" => "application/pdf",
            "jpg" | "jpeg" => "image/jpeg",
            "png" => "image/png",
            "json" => "application/json",
            "cue" | "txt" | "nfo" => "text/plain",
            _ => return None,
        }
        .to_string(),
    )
}

/// The upload manifest of an acquired title: one entry per distinct file
/// content (size and SHA-256), listing its key on every destination that
/// has it, with the local path its bytes are read from.
///
/// # Errors
///
/// Returns an error when a file cannot be read, a key is unusable, a
/// destination has no files, or there are too many files.
pub async fn upload_manifest(
    destinations: &[HostDestination],
    outbox: &Path,
    acquired: &AcquiredTitle,
    source_store: Option<&str>,
) -> anyhow::Result<Vec<(UploadFile, PathBuf)>> {
    let media = bookclerk_media::pool();
    upload_manifest_probed_by(destinations, outbox, acquired, source_store, &media).await
}

/// What `media`'s probe finds in the audio file at `path`, for the upload
/// manifest: its media type and its [`UploadProbe`], naming `source_store`
/// when that is a plain store id.
///
/// # Returns
///
/// `None` for a file that is not audio or cannot be probed (a format the
/// tag reader does not know, or no confined worker where one is required);
/// the file is uploaded either way.
async fn probe_for_upload(
    media: &bookclerk_media::MediaPool,
    key: &str,
    path: &Path,
    source_store: Option<&str>,
) -> Option<(String, UploadProbe)> {
    if !content_type(key).is_some_and(|kind| kind.starts_with("audio/")) {
        return None;
    }
    match media.probe_file(path).await {
        Ok(probe) => Some((
            probe.content_type.to_owned(),
            UploadProbe {
                bitrate_kbps: probe
                    .bitrate_kbps
                    .map(|kbps| kbps.min(bookclerk_node_proto::MAX_PROBE_BITRATE_KBPS)),
                lossless: probe.lossless,
                source_store: source_store
                    .filter(|store| bookclerk_node_proto::is_store_id(store))
                    .map(str::to_owned),
            },
        )),
        Err(err) => {
            tracing::debug!(key, error = %err, "acquired audio file was not probed");
            None
        }
    }
}

/// [`upload_manifest`] whose audio files are probed through `media`
/// instead of the process-wide pool.
///
/// # Errors
///
/// As [`upload_manifest`].
pub(crate) async fn upload_manifest_probed_by(
    destinations: &[HostDestination],
    outbox: &Path,
    acquired: &AcquiredTitle,
    source_store: Option<&str>,
    media: &bookclerk_media::MediaPool,
) -> anyhow::Result<Vec<(UploadFile, PathBuf)>> {
    if acquired.keys.len() != destinations.len() {
        anyhow::bail!("the acquire did not write every destination");
    }
    let mut files: Vec<(UploadFile, PathBuf)> = Vec::new();
    let mut by_content = std::collections::HashMap::new();
    for (index, (destination, keys)) in destinations.iter().zip(&acquired.keys).enumerate() {
        if keys.is_empty() {
            anyhow::bail!("destination {:?} received no file", destination.id);
        }
        let dir = destination_outbox(outbox, index);
        for key in keys {
            let path = dir.join(key);
            let (size, sha256) = digest_file(&path).await?;
            let copy = FileCopy {
                destination: destination.id.to_string(),
                key: key.clone(),
            };
            // A new manifest entry for this content, probed where it lies.
            let new_file = |copy: FileCopy, size: u64, sha256: String| {
                let path = path.clone();
                async move {
                    let probed = probe_for_upload(media, &copy.key, &path, source_store).await;
                    let (content_type, probe) = match probed {
                        Some((kind, probe)) => (Some(kind), Some(probe)),
                        None => (content_type(&copy.key), None),
                    };
                    UploadFile {
                        copies: vec![copy],
                        size,
                        sha256,
                        content_type,
                        probe,
                    }
                }
            };
            match by_content.get(&(size, sha256.clone())) {
                Some(&at) => {
                    let (file, _): &mut (UploadFile, PathBuf) = &mut files[at];
                    if file.key_on(destination.id.as_str()).is_some() {
                        // The same bytes twice on one destination: a
                        // separate entry, since a file has one copy per
                        // destination.
                        by_content.insert((size, sha256.clone()), files.len());
                        files.push((new_file(copy, size, sha256).await, path));
                    } else {
                        file.copies.push(copy);
                    }
                }
                None => {
                    by_content.insert((size, sha256.clone()), files.len());
                    files.push((new_file(copy, size, sha256).await, path));
                }
            }
        }
    }
    if files.is_empty() || files.len() > MAX_UPLOAD_FILES {
        anyhow::bail!("the title has {} files to upload", files.len());
    }
    if files.iter().any(|(file, _)| !file.is_well_formed()) {
        anyhow::bail!("an acquired file has an unusable storage key");
    }
    Ok(files)
}

/// One file this run wrote to one of this host's own destinations: the
/// destination handle it was written through and the key. The handle is
/// kept rather than re-read from [`HostGuests::local_destinations`], so a
/// destination deployed or retired before cleanup cannot redirect the
/// delete to a destination this run never wrote.
pub type LocalCopy = (Arc<dyn StorageBackend>, String);

/// Copies one title's files (the primary destination's keys) to every one
/// of this host's own destinations. Any failed copy fails the title. With
/// an `attempt`, each key is claimed in this install's own library before
/// it is written, and one another title holds there fails the title
/// instead of being overwritten. Each
/// copy this run created (its key was not there before) is put in `copied`
/// as soon as it is written, so the caller removes exactly what this run
/// created ([`remove_local_copies`]) whether this fails or is dropped
/// mid-copy; a key that was already there is never deleted by it.
///
/// # Returns
///
/// Every file now on this host's own destinations, created or overwritten:
/// `(destination id, key)`.
///
/// # Errors
///
/// Returns an error when the destinations cannot be used or a copy fails.
pub async fn copy_to_local_destinations<G: HostGuests>(
    guests: &G,
    book_uuid: &str,
    outbox: &Path,
    acquired: &AcquiredTitle,
    copied: &std::sync::Mutex<Vec<LocalCopy>>,
    attempt: Option<&CopyAttempt>,
) -> anyhow::Result<Vec<(DestinationId, String)>> {
    let local: Vec<(DestinationId, Arc<dyn StorageBackend>)> = guests
        .local_destinations()
        .await
        .context("this host's own destinations")?
        .into_iter()
        .map(|(id, backend)| (id, Arc::from(backend)))
        .collect();
    let mut placed = Vec::new();
    // This host's own copy follows the owner's primary destination naming.
    let primary = destination_outbox(outbox, 0);
    for key in acquired.keys.first().into_iter().flatten() {
        for (id, destination) in &local {
            let meta = ObjectMeta {
                content_type: content_type(key),
                ..ObjectMeta::default()
            };
            // Only a key this run creates is this run's to roll back: one
            // already there (an earlier acquire of the title, or another
            // writer's file) is overwritten as before but never deleted by
            // this run's cleanup. An unreadable probe counts as present,
            // so a failure leaves the file rather than risk another's.
            let existed = destination.exists(key).await.unwrap_or(true);
            // Recorded first, so a key another title holds is refused
            // rather than overwritten, and a crash mid-copy is found on
            // the next start (#314).
            if let Some(attempt) = attempt {
                let target = if existed {
                    control_plane::CopyTarget::Existing
                } else {
                    control_plane::CopyTarget::New
                };
                attempt.claim(id, key, target).await?;
            }
            if let Err(err) = destination.put_file(key, &primary.join(key), meta).await {
                tracing::warn!(book_uuid, %key, error = %err, "could not copy a title to this host's own destination");
                anyhow::bail!("this host's own destination did not take {key:?}: {err}");
            }
            if !existed {
                copied
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push((Arc::clone(destination), key.clone()));
            }
            placed.push((id.clone(), key.clone()));
        }
    }
    Ok(placed)
}

/// Deletes `copies` (the files one run wrote for a title) from this host's
/// own destinations, best effort: a title the owner did not commit keeps
/// no copy here. Skipped when `fence_lost`: the owner may have handed the
/// job to a replacement run, here or elsewhere, that writes the same keys,
/// and a host-side copy carries no writer identity to tell the two apart,
/// so a stale run's cleanup could delete the replacement's file. The
/// stale copy is left for the replacement to overwrite.
pub async fn remove_local_copies(book_uuid: &str, copies: &[LocalCopy], fence_lost: bool) {
    if copies.is_empty() {
        return;
    }
    if fence_lost {
        tracing::warn!(
            book_uuid,
            files = copies.len(),
            "the job's fence is lost; leaving an uncommitted title's copies on this host's own destinations to the run that holds it now"
        );
        return;
    }
    for (destination, key) in copies {
        if let Err(err) = destination.delete(key).await {
            tracing::warn!(book_uuid, %key, error = %err, "could not remove an uncommitted title's copy from this host's own destination");
        }
    }
}

/// True when the titles this job acquires are also kept on this host's own
/// destinations (H-2): always for the branch owner's own books, and for
/// another member's only when the link's "Keep copies of other members'
/// books" is on. Read once as the job starts acquiring.
async fn keeps_local_copy<G: HostGuests>(host: &HostJobCtx, guests: &G) -> bool {
    host.job.job.for_node_owner || guests.keeps_member_copies().await
}

/// Where a title kept on this host's own destinations tracks its copies:
/// the files it wrote (removed when the owner does not commit the title)
/// and, with a [`CopyLedger`], its [`CopyAttempt`].
#[derive(Clone, Copy)]
struct KeptCopy<'a> {
    /// Each copy this run created, as it is written.
    copied: &'a std::sync::Mutex<Vec<LocalCopy>>,
    /// The title's record in this install's library, when it keeps one.
    attempt: Option<&'a CopyAttempt>,
}

/// Copies one title's files to this host's own destinations (H-2) when
/// the job keeps them (`kept` is `Some`, [`keeps_local_copy`]), then
/// streams them to the owner and commits the title. A title it keeps is
/// uploaded only once every one of this host's destinations got it. Each
/// copy is put in `copied` as it is written, so the caller can remove them when
/// the owner does not commit the title ([`remove_local_copies`]), whether
/// this fails or is dropped. `commit_unsettled` turns true as the commit is
/// first sent and back to false only when the owner definitively refuses
/// it, so a caller seeing an error with it still true cannot tell whether
/// the owner committed the title and must keep the copies.
///
/// # Errors
///
/// Returns an error when the manifest, a local copy, the grant, a file or
/// the commit fails.
async fn upload_title<G: HostGuests>(
    guests: &G,
    host: &HostJobCtx,
    title: &HostTitle,
    outbox: &Path,
    acquired: &AcquiredTitle,
    kept: Option<KeptCopy<'_>>,
    commit_unsettled: &AtomicBool,
) -> anyhow::Result<()> {
    let attempt = kept.and_then(|kept| kept.attempt);
    let book_uuid = title.holding_id.as_str();
    let manifest = upload_manifest(
        &title.destinations,
        outbox,
        acquired,
        Some(title.book.source.as_str()),
    )
    .await?;
    let placed = match kept {
        Some(kept) => {
            copy_to_local_destinations(guests, book_uuid, outbox, acquired, kept.copied, attempt)
                .await?
        }
        None => Vec::new(),
    };
    let (files, paths): (Vec<UploadFile>, Vec<PathBuf>) = manifest.into_iter().unzip();
    let upload = match with_retries(|| {
        host.grant_upload(
            &title.holding_id,
            &acquired.primary_key,
            acquired.pdf_key.as_deref(),
            files.clone(),
        )
    })
    .await
    {
        Ok(upload) => upload,
        Err(OwnerCallError::Refused(reason)) if reason == "edition_acquired" => {
            // Another account's title of the same edition was committed
            // since the plan: the library shares those files with this
            // title, so it is done, not failed (#689). A copy kept here is
            // the edition's too.
            tracing::info!(
                job_id = %host.job.job.id,
                book_uuid,
                "another title of this edition reached the library first; sharing its files"
            );
            if let Some(attempt) = attempt {
                attempt.keep().await;
            }
            return Ok(());
        }
        Err(err) => return Err(owner_error("the upload", &err)),
    };
    for (index, (file, path)) in files.iter().zip(&paths).enumerate() {
        // Each attempt resumes where the owner stands with the file.
        let source = LocalFileSource(path.clone());
        with_retries(|| host.upload_file(&upload, index, file.size, &source))
            .await
            .map_err(|err| owner_error("an uploaded file", &err))?;
    }
    // An attempt that got no usable answer may still have committed the
    // title; a refusal after it then only answers the retry, so it does not
    // settle the outcome either.
    let ambiguous = AtomicBool::new(false);
    commit_unsettled.store(true, Ordering::SeqCst);
    let committed = with_retries(|| async {
        let out = host.commit_upload(&upload, None).await;
        if matches!(out, Err(OwnerCallError::Unavailable)) {
            ambiguous.store(true, Ordering::SeqCst);
        }
        out
    })
    .await;
    if matches!(committed, Err(OwnerCallError::Refused(_))) && !ambiguous.load(Ordering::SeqCst) {
        commit_unsettled.store(false, Ordering::SeqCst);
    }
    committed.map_err(|err| owner_error("the title's commit", &err))?;
    if let Some(attempt) = attempt {
        attempt.keep().await;
    }
    tracing::info!(
        job_id = %host.job.job.id,
        book_uuid,
        files = files.len(),
        "streamed a title to the owner"
    );
    // The owner's own book joins this install's library too, once the
    // owner holds it; another member's copy stays a plain file.
    if host.job.job.for_node_owner && !placed.is_empty() {
        guests
            .import_owner_title(OwnerTitleCopy {
                holding_id: &title.holding_id,
                book: &title.book,
                acquired,
                files: &placed,
            })
            .await;
    }
    Ok(())
}
