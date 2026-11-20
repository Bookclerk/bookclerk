//! The `ingest_batch` job: turns a committed batch's staged files into
//! patron-held editions in the library's storage.
//!
//! The job runs on the node that holds the batch's bytes. It checks each
//! member against its manifest, expands archives in the confined media
//! worker, groups the files into books ([`super::identify::group`]),
//! resolves each book through an import listing held by the uploader, and
//! writes the files to every deployed destination under the edition's
//! naming. Each member ends with an outcome the uploader can read back;
//! nothing is retried member by member.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail};
use bookclerk_acquire::{storage_key_with_contexts, NamingContext};
use bookclerk_archive::{ArchiveFormat, EntryPath, Limits, RefusalReason, StopReason, SNIFF_BYTES};
use bookclerk_config::Config;
use bookclerk_library::control_plane::{
    add_expanded_members, batch_members, finish_batch, load_batch, load_or_create_node_identity,
    patron_holding_id, record_ingest_acquired, set_member_outcomes, start_batch, ExpandedEntry,
    IngestBatch, IngestBatchId, IngestMember, IngestSource, IngestState, MemberOutcome,
    OutcomeUpdate,
};
use bookclerk_library::{call_number, AcquireStatus, FileOwner, JobPayload, LibraryStore};
use bookclerk_resolve::{Entitlement, Holder, HoldingDraft, LinkReason, Listing};
use bookclerk_storage::{ByteRange, ObjectMeta};
use chrono::Utc;
use serde::Serialize;
use tracing::warn;

use super::identify::{self, BookGroup, Candidate, FileKind};
use super::intake_copy::{self, CopyFailure};
use super::staging;
use crate::api::AppState;
use crate::job_handler::JobExecCtx;
use crate::node_api::uploads::{owner_destinations, OwnerDestination};

/// What one run did, as the job's result text.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct IngestSummary {
    /// Books stored under an edition a strong identifier named.
    pub matched: u32,
    /// Books stored under a new local edition for an Administrator to
    /// review.
    pub needs_review: u32,
    /// Books the library already had.
    pub duplicate: u32,
    /// Files refused (type, digest, archive entry rules).
    pub refused: u32,
    /// Files that could not be stored.
    pub failed: u32,
    /// Archives expanded.
    pub expanded: u32,
}

impl IngestSummary {
    /// Counts `updates` into the summary.
    fn count(&mut self, updates: &[OutcomeUpdate]) {
        for update in updates {
            match update.outcome {
                MemberOutcome::Matched => self.matched += 1,
                MemberOutcome::NeedsReview => self.needs_review += 1,
                MemberOutcome::Duplicate => self.duplicate += 1,
                MemberOutcome::Refused => self.refused += 1,
                MemberOutcome::Failed => self.failed += 1,
                MemberOutcome::Expanded => self.expanded += 1,
                MemberOutcome::Pending => {}
            }
        }
    }
}

/// An outcome for `member_no` with no edition.
fn outcome(member_no: u32, outcome: MemberOutcome, reason: &str) -> OutcomeUpdate {
    OutcomeUpdate {
        member_no,
        outcome,
        reason: Some(reason.to_string()),
        edition_id: None,
    }
}

/// Runs the `ingest_batch` job named by `payload`.
///
/// # Returns
///
/// The run's [`IngestSummary`] as JSON.
///
/// # Errors
///
/// Returns an error when the batch is missing, staged on another node or
/// not committed, when the library cannot be read or written, or when the
/// job is cancelled. The batch is marked failed (or cancelled) first.
pub(crate) async fn run_ingest_batch(
    state: &AppState,
    payload: &JobPayload,
    ctx: Option<&JobExecCtx>,
) -> anyhow::Result<String> {
    let raw = payload
        .ingest_batch
        .as_deref()
        .ok_or_else(|| anyhow!("ingest_batch job without a batch"))?;
    let id = IngestBatchId::parse(raw).ok_or_else(|| anyhow!("malformed ingest batch id"))?;
    let library = state.library_snapshot().await;
    let cfg = state.config.read().await.clone();
    let files_dir = cfg.paths().files_dir.clone();
    let node_id = load_or_create_node_identity(&files_dir)?.node_id;
    let batch = load_batch(&library, &id)
        .await?
        .ok_or_else(|| anyhow!("ingest batch {id} is gone"))?;
    if batch.node_id != node_id {
        bail!("ingest batch {id} is staged on another node");
    }
    let job_id = ctx
        .map(|ctx| ctx.fence.job_id.clone())
        .or_else(|| batch.job_id.clone())
        .unwrap_or_default();
    if !start_batch(&library, &id, &job_id).await? {
        bail!("ingest batch {id} is not committed");
    }
    let run = Run {
        state,
        library: &library,
        cfg: &cfg,
        files_dir: &files_dir,
        batch: &batch,
        ctx,
    };
    let result = run.ingest().await;
    let (final_state, detail) = match &result {
        Ok(_) => (IngestState::Done, None),
        Err(_) if ctx.is_some_and(JobExecCtx::is_cancelled) => {
            (IngestState::Cancelled, Some("cancelled".to_string()))
        }
        Err(err) => (IngestState::Failed, Some(err.to_string())),
    };
    finish_batch(&library, &id, final_state, detail.as_deref()).await?;
    if let Err(err) = staging::remove_batch(&files_dir, &id).await {
        warn!(batch = %id, error = %err, "could not remove a finished batch's staged files");
    }
    let summary = result?;
    Ok(serde_json::to_string(&summary)?)
}

/// One run of the job over one batch.
struct Run<'a> {
    /// Daemon state (destinations, media).
    state: &'a AppState,
    /// The library the batch lives in.
    library: &'a LibraryStore,
    /// The configuration the run started with.
    cfg: &'a Config,
    /// `$BOOKCLERK_FILES_DIR`.
    files_dir: &'a Path,
    /// The batch.
    batch: &'a IngestBatch,
    /// The job's context, for cancellation.
    ctx: Option<&'a JobExecCtx>,
}

impl Run<'_> {
    /// Fails when the job was cancelled.
    fn check_cancelled(&self) -> anyhow::Result<()> {
        if self.ctx.is_some_and(JobExecCtx::is_cancelled) {
            bail!("cancelled");
        }
        Ok(())
    }

    /// Records `updates` and counts them into `summary`.
    async fn record(
        &self,
        summary: &mut IngestSummary,
        updates: &[OutcomeUpdate],
    ) -> anyhow::Result<()> {
        if updates.is_empty() {
            return Ok(());
        }
        set_member_outcomes(self.library, &self.batch.id, updates).await?;
        summary.count(updates);
        Ok(())
    }

    /// The staged file of `member_no`.
    fn staged(&self, member_no: u32) -> PathBuf {
        staging::member_file(self.files_dir, &self.batch.id, member_no)
    }

    /// Runs every stage over the batch.
    async fn ingest(&self) -> anyhow::Result<IngestSummary> {
        let mut summary = IngestSummary::default();
        let members = batch_members(self.library, &self.batch.id).await?;
        let mut updates = Vec::new();
        let mut ready = Vec::new();
        for member in members
            .into_iter()
            .filter(|member| member.outcome == MemberOutcome::Pending && member.parent_no.is_none())
        {
            self.check_cancelled()?;
            match self.check_member(&member).await {
                Ok(()) => ready.push(member),
                Err(update) => updates.push(update),
            }
        }
        self.record(&mut summary, &updates).await?;
        let candidates = self.expand_archives(&mut summary, ready).await?;
        let (groups, refusals) = identify::group(&candidates);
        let updates: Vec<_> = refusals
            .iter()
            .map(|refusal| outcome(refusal.member_no, MemberOutcome::Refused, refusal.reason))
            .collect();
        self.record(&mut summary, &updates).await?;
        let destinations = match owner_destinations(self.state).await {
            Ok(destinations) if !destinations.is_empty() => destinations,
            Ok(_) | Err(_) => {
                let updates: Vec<_> = groups
                    .iter()
                    .flat_map(group_members)
                    .map(|member_no| outcome(member_no, MemberOutcome::Failed, "no_destination"))
                    .collect();
                self.record(&mut summary, &updates).await?;
                return Ok(summary);
            }
        };
        for group in &groups {
            self.check_cancelled()?;
            let updates = self.store_book(group, &destinations).await?;
            self.record(&mut summary, &updates).await?;
        }
        Ok(summary)
    }

    /// Brings `member`'s bytes into staging (an intake member) and checks
    /// them against the manifest.
    ///
    /// # Errors
    ///
    /// The member's refusal or failure.
    async fn check_member(&self, member: &IngestMember) -> Result<(), OutcomeUpdate> {
        let path = self.staged(member.member_no);
        if self.batch.source == IngestSource::Intake {
            self.copy_intake(member, &path).await?;
        }
        let len = tokio::fs::metadata(&path).await.map(|meta| meta.len());
        if len.ok() != Some(member.size) {
            return Err(outcome(
                member.member_no,
                MemberOutcome::Failed,
                "missing_bytes",
            ));
        }
        if let Some(expected) = &member.sha256 {
            match staging::sha256_file(&path).await {
                Ok(actual) if actual == *expected => {}
                Ok(_) => {
                    return Err(outcome(
                        member.member_no,
                        MemberOutcome::Refused,
                        "digest_mismatch",
                    ))
                }
                Err(_) => {
                    return Err(outcome(
                        member.member_no,
                        MemberOutcome::Failed,
                        "unreadable",
                    ))
                }
            }
        }
        Ok(())
    }

    /// Copies an intake member from its drop folder (deployed on this node,
    /// by instance id) into staging.
    ///
    /// # Errors
    ///
    /// `intake_unavailable` when the drop folder is not deployed here or the
    /// object cannot be read; `intake_changed` when the object no longer
    /// has the size it settled with (it is still being written). A read
    /// that breaks partway resumes from the staged length, a few times with
    /// backoff, before the member fails.
    async fn copy_intake(&self, member: &IngestMember, path: &Path) -> Result<(), OutcomeUpdate> {
        let unavailable = || {
            outcome(
                member.member_no,
                MemberOutcome::Failed,
                "intake_unavailable",
            )
        };
        let (Some(instance), Some(key)) = (
            self.batch.intake_instance.as_deref(),
            member.storage_key.as_deref(),
        ) else {
            return Err(unavailable());
        };
        let Some(source) = intake_source(self.state, instance).await else {
            return Err(unavailable());
        };
        // The copy caps each body itself; a range never reaches past the
        // end, which backends refuse.
        let open = |offset: u64, _length: u64| {
            let source = Arc::clone(&source);
            async move {
                let range = (offset > 0).then_some(ByteRange {
                    offset,
                    length: None,
                });
                source
                    .get_stream(key, range)
                    .await
                    .map(|(_, body)| body)
                    .map_err(std::io::Error::other)
            }
        };
        match intake_copy::copy_resumable(open, path, member.size, INTAKE_COPY_BACKOFF).await {
            Ok(()) => Ok(()),
            Err(CopyFailure::Unavailable) => Err(unavailable()),
            Err(CopyFailure::Changed) => Err(outcome(
                member.member_no,
                MemberOutcome::Failed,
                "intake_changed",
            )),
        }
    }

    /// Expands the archives among `ready` and returns every file left to
    /// group: the plain members and the archives' entries.
    async fn expand_archives(
        &self,
        summary: &mut IngestSummary,
        ready: Vec<IngestMember>,
    ) -> anyhow::Result<Vec<Candidate>> {
        let mut candidates = Vec::new();
        for member in ready {
            self.check_cancelled()?;
            let path = self.staged(member.member_no);
            let head = staging::head(&path, SNIFF_BYTES).await.unwrap_or_default();
            if ArchiveFormat::sniff(&head).is_none() {
                if ArchiveFormat::looks_like_archive(&head) {
                    let refused = outcome(
                        member.member_no,
                        MemberOutcome::Refused,
                        "unsupported_archive",
                    );
                    self.record(summary, &[refused]).await?;
                } else {
                    candidates.push(Candidate {
                        member_no: member.member_no,
                        path: member.path,
                    });
                }
                continue;
            }
            match self.expand(&member, &path).await? {
                Ok(children) => {
                    summary.expanded += 1;
                    candidates.extend(children);
                }
                Err(failed) => self.record(summary, &[failed]).await?,
            }
        }
        Ok(candidates)
    }

    /// Expands archive `member` in the media worker, records its entries
    /// as new members and moves their bytes into staging.
    ///
    /// # Returns
    ///
    /// The written entries to group, or the archive's failure.
    async fn expand(
        &self,
        member: &IngestMember,
        path: &Path,
    ) -> anyhow::Result<Result<Vec<Candidate>, OutcomeUpdate>> {
        let uploads = &self.cfg.uploads;
        if staging::below_floor(self.files_dir, member.size, uploads.free_disk_floor()) {
            return Ok(Err(outcome(
                member.member_no,
                MemberOutcome::Failed,
                "disk_full",
            )));
        }
        let limits = Limits {
            max_entry_bytes: Limits::UPLOAD_DEFAULTS
                .max_entry_bytes
                .min(uploads.max_file_bytes),
            ..Limits::UPLOAD_DEFAULTS.with_total_at_most(uploads.max_staged_bytes)
        };
        let out = staging::extract_dir(self.files_dir, &self.batch.id, member.member_no);
        let report = match bookclerk_media::extract_archive(path, &out, limits).await {
            Ok(report) => report,
            Err(err) => {
                warn!(batch = %self.batch.id, member = member.member_no, error = %err, "archive expansion failed");
                let _ = tokio::fs::remove_dir_all(&out).await;
                let reason = if matches!(err, bookclerk_media::MediaError::NotIsolated { .. }) {
                    "media_worker_unavailable"
                } else {
                    "archive_unreadable"
                };
                return Ok(Err(outcome(
                    member.member_no,
                    MemberOutcome::Failed,
                    reason,
                )));
            }
        };
        if let Some(stop) = report.stopped {
            let _ = tokio::fs::remove_dir_all(&out).await;
            return Ok(Err(outcome(
                member.member_no,
                MemberOutcome::Failed,
                stop_code(stop),
            )));
        }
        let stem = archive_stem(&member.path);
        let mut entries = Vec::with_capacity(report.written.len() + report.refused.len());
        let mut sources = Vec::with_capacity(entries.capacity());
        for written in &report.written {
            match EntryPath::admit(&format!("{stem}/{}", written.path)) {
                Ok(child) => {
                    entries.push(ExpandedEntry {
                        path: child.as_str().to_string(),
                        size: written.bytes,
                        outcome: MemberOutcome::Pending,
                        reason: None,
                    });
                    sources.push(Some(written.path.clone()));
                }
                Err(_) => {
                    entries.push(ExpandedEntry {
                        path: written.path.as_str().to_string(),
                        size: 0,
                        outcome: MemberOutcome::Refused,
                        reason: Some("path".to_string()),
                    });
                    sources.push(None);
                }
            }
        }
        for refused in &report.refused {
            entries.push(ExpandedEntry {
                path: refused.name.clone(),
                size: 0,
                outcome: MemberOutcome::Refused,
                reason: Some(refusal_code(refused.reason).to_string()),
            });
            sources.push(None);
        }
        let first = add_expanded_members(
            self.library,
            &self.batch.id,
            member.member_no,
            &entries,
            None,
        )
        .await?;
        let mut children = Vec::new();
        for (offset, (entry, source)) in entries.iter().zip(sources).enumerate() {
            let Some(source) = source else { continue };
            let child_no = first + u32::try_from(offset)?;
            let from = source
                .components()
                .fold(out.clone(), |dir, part| dir.join(part));
            let to = self.staged(child_no);
            if let Some(parent) = to.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            tokio::fs::rename(&from, &to).await?;
            children.push(Candidate {
                member_no: child_no,
                path: entry.path.clone(),
            });
        }
        let _ = tokio::fs::remove_dir_all(&out).await;
        let _ = tokio::fs::remove_file(path).await;
        Ok(Ok(children))
    }

    /// Resolves `group` into an edition held by the uploader and writes its
    /// files to every destination.
    ///
    /// # Returns
    ///
    /// The outcome of each of the group's members.
    async fn store_book(
        &self,
        group: &BookGroup,
        destinations: &[OwnerDestination],
    ) -> anyhow::Result<Vec<OutcomeUpdate>> {
        let all = |result: MemberOutcome, reason: Option<&str>, edition_id: Option<&str>| {
            group_members(group)
                .map(|member_no| OutcomeUpdate {
                    member_no,
                    outcome: result,
                    reason: reason.map(str::to_string),
                    edition_id: edition_id.map(str::to_string),
                })
                .collect::<Vec<_>>()
        };
        let Some(first) = group.audio.first() else {
            return Ok(Vec::new());
        };
        let first_path = self.staged(first.member_no);
        let size = tokio::fs::metadata(&first_path).await?.len();
        let tags = bookclerk_media::pool()
            .read_tags(size, &bookclerk_media::FileRange::new(&first_path))
            .await
            .ok();
        let evidence = identify::evidence(group, tags.as_ref(), group.audio.len());
        let digest = staging::sha256_file(&first_path).await?;
        let product_id = digest.get(..32).unwrap_or(&digest);
        let draft = identify::listing_draft(&evidence, product_id);
        let Ok(listing) = Listing::from_draft(&draft) else {
            return Ok(all(MemberOutcome::Failed, Some("unlisted"), None));
        };
        let plan = self
            .library
            .record_listing(
                &listing,
                &HoldingDraft {
                    holder: Holder::Patron(self.batch.user_id),
                    entitlement: Entitlement::Own,
                    purchased_at: Some(Utc::now().to_rfc3339()),
                },
            )
            .await?;
        let edition_id = plan.listing.edition_id.clone();
        let named = match plan.link {
            LinkReason::Pinned | LinkReason::Listing | LinkReason::StrongIdentifier => true,
            LinkReason::WeakIdentifier | LinkReason::New => false,
        };
        let result = if named || evidence.identified() {
            MemberOutcome::Matched
        } else {
            MemberOutcome::NeedsReview
        };
        if self
            .library
            .edition_acquisition(&edition_id)
            .await?
            .is_some_and(|acquired| acquired.status == AcquireStatus::Acquired)
        {
            return Ok(all(MemberOutcome::Duplicate, None, Some(&edition_id)));
        }
        let Some(holding_id) =
            patron_holding_id(self.library, &plan.listing.id, self.batch.user_id).await?
        else {
            return Ok(all(
                MemberOutcome::Failed,
                Some("holding_missing"),
                Some(&edition_id),
            ));
        };
        let context = naming_context(&evidence, &edition_id);
        let mut primary = None;
        for destination in destinations {
            let files = self.plan_files(group, destination, &context);
            for file in &files {
                let owner = FileOwner::Edition(edition_id.clone());
                if !self
                    .library
                    .record_edition_file(destination.id.as_str(), &file.key, &owner, file.role)
                    .await?
                {
                    return Ok(all(
                        MemberOutcome::Failed,
                        Some("key_taken"),
                        Some(&edition_id),
                    ));
                }
                if let Err(err) = self.write(destination, file, &evidence.title).await {
                    warn!(batch = %self.batch.id, destination = %destination.id, key = %file.key, error = %err, "storing an uploaded file failed");
                    return Ok(all(
                        MemberOutcome::Failed,
                        Some("storage_write_failed"),
                        Some(&edition_id),
                    ));
                }
            }
            if primary.is_none() {
                primary = files.first().map(|file| file.key.clone());
            }
        }
        let Some(primary) = primary else {
            return Ok(all(
                MemberOutcome::Failed,
                Some("no_destination"),
                Some(&edition_id),
            ));
        };
        if !record_ingest_acquired(self.library, &edition_id, &holding_id, &primary).await? {
            return Ok(all(
                MemberOutcome::Failed,
                Some("holding_missing"),
                Some(&edition_id),
            ));
        }
        Ok(all(result, None, Some(&edition_id)))
    }

    /// The keys `group`'s files take on `destination`: the first audio file
    /// under the edition's naming, the rest as numbered parts, and extras
    /// beside them under their own names.
    fn plan_files(
        &self,
        group: &BookGroup,
        destination: &OwnerDestination,
        context: &NamingContext,
    ) -> Vec<PlannedFile> {
        let options = &destination.options;
        let templates = options.naming_templates();
        let key_for = |ctx: &NamingContext, ext: &str| {
            storage_key_with_contexts(
                context,
                ctx,
                Some(templates.folder.as_str()),
                Some(templates.file.as_str()),
                ext,
                &options.replacement_characters,
                options.path_limits,
            )
        };
        let mut files = Vec::new();
        let mut folder = String::new();
        for (index, candidate) in group.audio.iter().enumerate() {
            let ext = extension(&candidate.path);
            let key = if index == 0 {
                key_for(context, &ext)
            } else {
                let mut part = context.clone();
                let number = u32::try_from(index + 1).unwrap_or(u32::MAX);
                part.call_number = part.call_number.map(|id| format!("{id}-p{number:03}"));
                part.asin = format!("{}-p{number:03}", part.asin);
                part.chapter_number = Some(number);
                key_for(&part, &ext)
            };
            if index == 0 {
                folder = key
                    .rsplit_once('/')
                    .map_or_else(String::new, |(dir, _)| dir.to_string());
            }
            files.push(PlannedFile {
                member_no: candidate.member_no,
                key,
                role: FileKind::Audio.role(index == 0),
                content_type: content_type(&ext),
            });
        }
        for (candidate, kind) in &group.extras {
            let name = safe_name(&candidate.path);
            let key = if folder.is_empty() {
                name
            } else {
                format!("{folder}/{name}")
            };
            files.push(PlannedFile {
                member_no: candidate.member_no,
                key,
                role: kind.role(false),
                content_type: content_type(&extension(&candidate.path)),
            });
        }
        files
    }

    /// Streams one staged file to `destination`.
    async fn write(
        &self,
        destination: &OwnerDestination,
        file: &PlannedFile,
        title: &str,
    ) -> anyhow::Result<()> {
        let path = self.staged(file.member_no);
        let handle = tokio::fs::File::open(&path).await?;
        let len = handle.metadata().await?.len();
        let meta = ObjectMeta {
            content_type: Some(file.content_type.to_string()),
            content_length: Some(len),
            title: Some(title.to_string()),
            ..ObjectMeta::default()
        };
        let written = destination
            .backend
            .put_stream(&file.key, Box::pin(handle), meta)
            .await?;
        if written.bytes_written != len {
            bail!("wrote {} of {len} bytes", written.bytes_written);
        }
        Ok(())
    }
}

/// One file of a book and where it goes on one destination.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PlannedFile {
    /// The member whose bytes it is.
    member_no: u32,
    /// Its storage key.
    key: String,
    /// Its `edition_files` role.
    role: bookclerk_library::FileRole,
    /// Its MIME type.
    content_type: &'static str,
}

/// Every member of `group`, audio first.
fn group_members(group: &BookGroup) -> impl Iterator<Item = u32> + '_ {
    group
        .audio
        .iter()
        .map(|candidate| candidate.member_no)
        .chain(
            group
                .extras
                .iter()
                .map(|(candidate, _)| candidate.member_no),
        )
}

/// The naming context of the edition the evidence resolved to.
fn naming_context(evidence: &identify::Evidence, edition_id: &str) -> NamingContext {
    let join = |names: &[String]| (!names.is_empty()).then(|| names.join(", "));
    NamingContext {
        asin: evidence.asin.clone().unwrap_or_default(),
        title: evidence.title.clone(),
        subtitle: evidence.subtitle.clone(),
        authors: join(&evidence.authors),
        narrators: join(&evidence.narrators),
        series: evidence.series.clone(),
        series_index: evidence.series_position.clone(),
        year_published: evidence
            .published_at
            .as_deref()
            .and_then(|date| date.get(..4))
            .and_then(|year| year.parse().ok()),
        language: evidence.language.clone(),
        length_minutes: evidence.duration_minutes,
        call_number: call_number(edition_id),
        ..NamingContext::default()
    }
}

/// The lower-case extension of `path`, or `bin` when it has none.
fn extension(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.rsplit_once('.')
        .map_or_else(|| "bin".to_string(), |(_, ext)| ext.to_ascii_lowercase())
}

/// The MIME type stored with a file of extension `ext`.
fn content_type(ext: &str) -> &'static str {
    match ext {
        "mp3" => "audio/mpeg",
        "m4a" | "m4b" | "mp4" => "audio/mp4",
        "aac" => "audio/aac",
        "flac" => "audio/flac",
        "ogg" | "oga" | "opus" => "audio/ogg",
        "wav" => "audio/wav",
        "epub" => "application/epub+zip",
        "pdf" => "application/pdf",
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "webp" => "image/webp",
        "txt" | "cue" | "nfo" | "chapters" | "lrc" | "srt" => "text/plain",
        "vtt" => "text/vtt",
        "json" => "application/json",
        "xml" | "opf" => "application/xml",
        _ => "application/octet-stream",
    }
}

/// The last part of `path` with anything but letters, digits, spaces and
/// `.`, `-`, `_`, `(`, `)` replaced, never empty and never hidden.
fn safe_name(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, ' ' | '.' | '-' | '_' | '(' | ')') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let cleaned = cleaned.trim_start_matches('.').trim().to_string();
    if cleaned.is_empty() {
        "file".to_string()
    } else {
        cleaned
    }
}

/// The folder an archive's entries go under: its path without the
/// extension (`Books/Dune.zip` becomes `Books/Dune`; `x.tar.gz` becomes
/// `x`).
fn archive_stem(path: &str) -> String {
    let (folder, name) = path
        .rsplit_once('/')
        .map_or(("", path), |(folder, name)| (folder, name));
    let lower = name.to_ascii_lowercase();
    let cut = [".tar.gz", ".tgz", ".zip", ".tar"]
        .iter()
        .find(|ext| lower.ends_with(*ext))
        .map_or(name.len(), |ext| name.len() - ext.len());
    let stem = match name.get(..cut) {
        Some(stem) if !stem.is_empty() => stem,
        _ => "archive",
    };
    if folder.is_empty() {
        stem.to_string()
    } else {
        format!("{folder}/{stem}")
    }
}

/// The stable reason of a refused archive entry.
fn refusal_code(reason: RefusalReason) -> &'static str {
    match reason {
        RefusalReason::Path { .. } => "path",
        RefusalReason::Clash { .. } => "path_clash",
        RefusalReason::Link => "link",
        RefusalReason::Special => "special_file",
        RefusalReason::Encrypted => "encrypted",
        RefusalReason::Compression => "unsupported_compression",
        RefusalReason::NestedArchive => "nested_archive",
        RefusalReason::EntryTooLarge => "too_large",
        RefusalReason::RatioExceeded => "ratio_exceeded",
        RefusalReason::Overlap => "overlap",
        RefusalReason::SystemFile => "system_file",
        RefusalReason::Corrupt => "corrupt",
        RefusalReason::WriteFailed => "write_failed",
    }
}

/// The stable reason of an archive whose walk stopped.
fn stop_code(stop: StopReason) -> &'static str {
    match stop {
        StopReason::TooManyEntries => "archive_too_many_entries",
        StopReason::TotalExceeded => "archive_too_large",
        StopReason::Corrupt => "archive_corrupt",
    }
}

/// Wait before the first resumption of a broken drop folder read; it
/// doubles for each one after.
const INTAKE_COPY_BACKOFF: Duration = Duration::from_millis(500);

/// The storage an intake batch reads: the drop folder with that plugin
/// instance id deployed on this node.
///
/// Drop folders are never acquire destinations, so they are looked up
/// among the registry's intakes, not its destinations.
///
/// # Returns
///
/// `None` when no drop folder with that id runs here.
async fn intake_source(
    state: &AppState,
    instance: &str,
) -> Option<std::sync::Arc<dyn bookclerk_storage::StorageBackend>> {
    let deployed = state
        .destinations
        .read()
        .await
        .intakes()
        .into_iter()
        .find(|intake| intake.id.as_str() == instance)
        .map(|intake| intake.backend);
    #[cfg(test)]
    if deployed.is_none() {
        // Tests stand local folders in for guests they cannot spawn.
        let files_dir = state.config.read().await.paths().files_dir.clone();
        let root = crate::node_api::uploads::test_intake(&files_dir, instance)?;
        let backend: std::sync::Arc<dyn bookclerk_storage::StorageBackend> =
            std::sync::Arc::new(bookclerk_storage::LocalFsBackend::new(root).ok()?);
        return Some(backend);
    }
    deployed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_entries_go_under_the_archive_name() {
        assert_eq!(archive_stem("Dune.zip"), "Dune");
        assert_eq!(archive_stem("Books/Dune.TAR.GZ"), "Books/Dune");
        assert_eq!(archive_stem("Books/x.tgz"), "Books/x");
        assert_eq!(archive_stem(".zip"), "archive");
        assert_eq!(archive_stem("noext"), "noext");
    }

    #[test]
    fn extras_keep_safe_names() {
        assert_eq!(safe_name("Book/cover art?.jpg"), "cover art_.jpg");
        assert_eq!(safe_name("Book/.hidden"), "hidden");
        assert_eq!(safe_name("Book/..."), "file");
        assert_eq!(extension("Book/A.M4B"), "m4b");
        assert_eq!(extension("Book/none"), "bin");
        assert_eq!(content_type("m4b"), "audio/mp4");
        assert_eq!(content_type("exe"), "application/octet-stream");
    }

    #[test]
    fn summaries_count_each_outcome() {
        let mut summary = IngestSummary::default();
        summary.count(&[
            outcome(0, MemberOutcome::Matched, "x"),
            outcome(1, MemberOutcome::NeedsReview, "x"),
            outcome(2, MemberOutcome::Refused, "x"),
            outcome(3, MemberOutcome::Pending, "x"),
        ]);
        assert_eq!(
            summary,
            IngestSummary {
                matched: 1,
                needs_review: 1,
                refused: 1,
                ..IngestSummary::default()
            }
        );
    }

    #[test]
    fn the_naming_context_carries_the_call_number() {
        let evidence = identify::Evidence {
            title: "Dune".into(),
            authors: vec!["Frank Herbert".into(), "Brian Herbert".into()],
            published_at: Some("1965-08-01T00:00:00Z".into()),
            ..identify::Evidence::default()
        };
        let id = "0b8f6a52-3c1d-4e2f-9a7b-6c5d4e3f2a1b";
        let context = naming_context(&evidence, id);
        assert_eq!(context.call_number, call_number(id));
        assert_eq!(
            context.authors.as_deref(),
            Some("Frank Herbert, Brian Herbert")
        );
        assert_eq!(context.year_published, Some(1965));
    }
}
