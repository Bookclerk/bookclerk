//! What a host's scan and acquire send back over the node socket (H-4
//! step 9; decisions 4 and 6).
//!
//! | Frame | Answer |
//! |---|---|
//! | `scan.results` | `scan.accepted {batch, upserted}`, `lost`, or `error {code: "refused"}` |
//! | `scan.removed` | `scan.accepted {batch, upserted}` (holdings removed), `lost`, or `error {code: "refused"}` |
//! | `scan.pass_end` | `scan.accepted {batch: 0, upserted}` (holdings removed), `lost`, or `error {code: "refused"}` |
//! | `acquire.plan` | `acquire.targets {targets, destinations}`, `lost`, or `error` |
//! | `upload.grant` | `upload.granted {upload_id, sealed_token, expires_in_secs}`, `lost`, or `error` |
//!
//! A scan's rows are parsed as the guest's `ScanBook` and handed to
//! [`accept_scan_rows`] (a record that maps onto no row goes as a sighting,
//! so the pass still counts it listed), which checks the fence, the node's trust and
//! every row's account on each batch before it writes any row; the host
//! never writes library SQL. After its rows, each account's pass reports
//! the ids a delta pass found gone ([`accept_scan_removed`]) and how it
//! ended ([`accept_scan_pass_end`]: its sync token, and the prune after a
//! complete full pass), under the same checks. An acquire asks for its titles (the owner's
//! pending rows the job covers, with every destination deployed on this
//! node and that destination's own options, so the host names each
//! destination's files as a local acquire would), then for each title asks
//! for an upload whose manifest gives each file's key per destination (each
//! key must lie in the title's own scope there, which this node computes
//! from its own row, and be no other title's file); the
//! token is sealed to the host's box key and bound to the node, job, fence
//! and upload, so it is never frame text.

use bookclerk_library::control_plane::{
    accept_scan_pass_end, accept_scan_removed, accept_scan_rows, grant_upload, job_titles,
    live_remote_job, HandedPassEnd, ScanAccept, ScanSighting, TitleNames, UploadDestination,
    UploadGrant, UploadGrantRequest,
};
use bookclerk_library::{AcquireTitle, JobKind, LibraryStore};
use bookclerk_node_proto::{
    seal_upload_token, AcquireTarget, Body, CallNumber, Frame, PlanDestination, TargetTitle,
    MAX_FRAME_BYTES, MAX_SCAN_BATCH_ROWS, MAX_SCAN_ROW_BYTES, MAX_UPLOAD_FILES,
};
use rand::RngCore as _;
use zeroize::Zeroize as _;

use super::work::{error, lost, HostContext};

/// Most titles one `acquire.targets` answer carries; the host asks again
/// for the rest.
const MAX_PLAN_TARGETS: usize = 100;

/// One `scan.results` batch as the host sent it.
pub(super) struct ScanBatch<'a> {
    /// Plugin whose scan produced the rows.
    pub plugin_key: &'a str,
    /// The host's batch counter, echoed in the answer.
    pub batch: u64,
    /// The rows, each a `ScanBook`.
    pub rows: Vec<serde_json::Value>,
}

/// `scan.results`: validates and writes one batch of a host scan's rows.
pub(super) async fn scan_results(
    library: &LibraryStore,
    host: &HostContext,
    id: String,
    job_id: String,
    fence: i64,
    scan: ScanBatch<'_>,
) -> bookclerk_library::Result<Frame> {
    let ScanBatch {
        plugin_key,
        batch,
        rows,
    } = scan;
    if rows.len() > MAX_SCAN_BATCH_ROWS {
        return Ok(error(id, "refused", "batch_too_large"));
    }
    let mut books = Vec::with_capacity(rows.len());
    // Records that map onto no flat row (an ebook, say) were still listed,
    // so they are carried as sightings: a complete full pass must not prune
    // them.
    let mut sightings = Vec::new();
    for row in rows {
        if row.to_string().len() > MAX_SCAN_ROW_BYTES {
            return Ok(error(id, "refused", "row_too_large"));
        }
        let Ok(book) = serde_json::from_value::<bookclerk_plugin_abi::ScanBook>(row) else {
            return Ok(error(id, "refused", "invalid_row"));
        };
        let sighting = ScanSighting {
            account_id: book.account_id.clone(),
            product_id: book.product_id.clone(),
        };
        match bookclerk_source::abi::scanned_title(plugin_key, book) {
            Some(title) => books.push(title),
            None => sightings.push(sighting),
        }
    }
    Ok(
        match accept_scan_rows(
            library,
            &host.node_id,
            &job_id,
            fence,
            plugin_key,
            books,
            sightings,
        )
        .await?
        {
            ScanAccept::Accepted { upserted } => {
                tracing::info!(
                    node_id = %host.node_id,
                    job_id = %job_id,
                    fence,
                    batch,
                    upserted,
                    "wrote a host's scan results"
                );
                Frame::new(id, Body::ScanAccepted { batch, upserted })
            }
            ScanAccept::Lost => lost(id, job_id, fence),
            ScanAccept::Refused(reason) => {
                tracing::warn!(
                    node_id = %host.node_id,
                    job_id = %job_id,
                    fence,
                    batch,
                    reason,
                    "refused a host's scan results"
                );
                error(id, "refused", reason)
            }
        },
    )
}

/// One account's report after a host scan's rows: removals or its end.
pub(super) enum PassReport<'a> {
    /// `scan.removed`: product ids a delta pass found gone.
    Removed {
        /// The host's batch counter, echoed in the answer.
        batch: u64,
        /// The ids.
        product_ids: &'a [String],
    },
    /// `scan.pass_end`: how the pass ended.
    End(HandedPassEnd<'a>),
}

/// One account's report as the host sent it.
pub(super) struct ScanPass<'a> {
    /// The job's source plugin.
    pub plugin_key: &'a str,
    /// The account the report is about.
    pub account_id: &'a str,
    /// What it reports.
    pub report: PassReport<'a>,
}

/// `scan.removed` / `scan.pass_end`: applies one account's report.
pub(super) async fn scan_pass_report(
    library: &LibraryStore,
    host: &HostContext,
    id: String,
    job_id: String,
    fence: i64,
    pass: ScanPass<'_>,
) -> bookclerk_library::Result<Frame> {
    let ScanPass {
        plugin_key,
        account_id,
        report,
    } = pass;
    let (batch, accepted) = match report {
        PassReport::Removed { batch, product_ids } => (
            batch,
            accept_scan_removed(
                library,
                &host.node_id,
                &job_id,
                fence,
                plugin_key,
                account_id,
                product_ids,
            )
            .await?,
        ),
        PassReport::End(end) => (
            0,
            accept_scan_pass_end(library, &host.node_id, &job_id, fence, plugin_key, end).await?,
        ),
    };
    Ok(match accepted {
        ScanAccept::Accepted { upserted } => {
            tracing::info!(
                node_id = %host.node_id,
                job_id = %job_id,
                fence,
                batch,
                removed = upserted,
                "applied a host scan's pass report"
            );
            Frame::new(id, Body::ScanAccepted { batch, upserted })
        }
        ScanAccept::Lost => lost(id, job_id, fence),
        ScanAccept::Refused(reason) => {
            tracing::warn!(
                node_id = %host.node_id,
                job_id = %job_id,
                fence,
                batch,
                reason,
                "refused a host scan's pass report"
            );
            error(id, "refused", reason)
        }
    })
}

/// The podcast parent of `title`, when `title` is an episode whose show the
/// same store account holds here: the title an acquire names an episode's
/// folder from when episodes are saved to the parent's folder.
///
/// # Errors
///
/// Returns the store's read error.
async fn podcast_parent(
    library: &LibraryStore,
    title: &AcquireTitle,
) -> bookclerk_library::Result<Option<AcquireTitle>> {
    if !bookclerk_library::is_episode(title.content_kind) {
        return Ok(None);
    }
    let Some(show) = title.series_product_id.as_deref() else {
        return Ok(None);
    };
    Ok(library
        .acquire_titles_named(Some(&title.account_id), show)
        .await?
        .into_iter()
        .find(|parent| {
            parent.plugin_key == title.plugin_key
                && parent.account_id == title.account_id
                && bookclerk_library::is_podcast_parent(parent.content_kind)
        }))
}

/// `title` as an acquire target names it ([`TargetTitle`]).
fn target_title(title: &AcquireTitle) -> TargetTitle {
    TargetTitle {
        product_id: title.product_id.clone(),
        source: title.source.clone(),
        marketplace: title.marketplace.clone(),
        asin: title.asin.clone(),
        isbn: title.isbn.clone(),
        title: title.title.clone(),
        subtitle: title.subtitle.clone(),
        authors: title.authors.clone(),
        narrators: title.narrators.clone(),
        series: title.series.clone(),
        series_index: title.series_index.clone(),
        series_product_id: title.series_product_id.clone(),
        publisher: title.publisher.clone(),
        categories: title.categories.clone(),
        length_minutes: title.length_minutes,
        is_abridged: title.is_abridged,
        content_kind: title.content_kind.to_string(),
        purchased_at: title.purchased_at.map(|at| at.to_rfc3339()),
        published_at: title.published_at.map(|at| at.to_rfc3339()),
    }
}

/// The `<call number>` of `edition_id`, as a target carries it.
fn target_call_number(edition_id: &str) -> Option<CallNumber> {
    bookclerk_library::call_number(edition_id).and_then(|id| CallNumber::try_from(id).ok())
}

/// `acquire.plan`: the titles the job still needs, and how to name them.
pub(super) async fn acquire_plan(
    library: &LibraryStore,
    host: &HostContext,
    id: String,
    job_id: String,
    fence: i64,
) -> bookclerk_library::Result<Frame> {
    let Some(job) = live_remote_job(library, &host.node_id, &job_id, fence).await? else {
        return Ok(lost(id, job_id, fence));
    };
    if job.kind != JobKind::Acquire {
        return Ok(error(id, "refused", "not_an_acquire"));
    }
    let destinations = match super::uploads::owner_destinations(&host.state).await {
        Ok(destinations) if !destinations.is_empty() => destinations,
        _ => return Ok(error(id, "unavailable", "no_destination")),
    };
    let download_episodes = host.state.config.read().await.library.download_episodes;
    // The same scope the closing commit checks before it completes the job.
    let Some(titles) = job_titles(library, &host.node_id, &job).await? else {
        return Ok(lost(id, job_id, fence));
    };
    let mut targets = Vec::new();
    for title in titles {
        if targets.len() >= MAX_PLAN_TARGETS {
            break;
        }
        // Only titles whose audio is not acquired on their edition, exactly
        // what the closing commit waits for and what a local acquire
        // visits: an edition another account's title acquired is already
        // in the library, so no host is sent to fetch it again. A title
        // whose audio is acquired but whose companion PDF is missing is left
        // to an acquire on a node holding the library: a host would fetch
        // and upload the audio again, and a missing PDF is a soft failure
        // that never keeps a job open.
        if !crate::jobs::acquire_job_includes_title(&title, false, download_episodes) {
            continue;
        }
        let parent = podcast_parent(library, &title).await?;
        // The host names with this library's edition ids, so it plans the
        // same keys this node grants.
        targets.push(AcquireTarget {
            holding_id: title.holding_id.clone(),
            plugin_key: title.plugin_key.clone(),
            account_id: title.account_id.clone(),
            title: target_title(&title),
            call_number: target_call_number(&title.edition_id),
            parent_call_number: parent
                .as_ref()
                .and_then(|parent| target_call_number(&parent.edition_id)),
            parent: parent.as_ref().map(target_title),
        });
    }
    let destinations = destinations
        .iter()
        .map(|destination| PlanDestination {
            id: destination.id.clone(),
            options: serde_json::to_value(&destination.options).unwrap_or_default(),
        })
        .collect();
    let mut frame = Frame::new(
        id.clone(),
        Body::AcquireTargets {
            targets,
            destinations,
        },
    );
    // Cut the list until it fits a frame; the host asks again for the rest.
    while frame.to_text().len() > MAX_FRAME_BYTES {
        let Body::AcquireTargets { targets, .. } = &mut frame.body else {
            break;
        };
        if targets.len() <= 1 {
            return Ok(error(id, "refused", "title_too_large"));
        }
        targets.truncate(targets.len() / 2);
    }
    Ok(frame)
}

/// This node's destinations with the key scope of `title` on each: the
/// folders a local acquire of the title would write under, named with that
/// destination's own options from this node's holding of it,
/// and, where that folder is shared with other titles or is the root, the
/// title's own planned names there
/// ([`bookclerk_acquire::planned_title_names`]). A title this node does not
/// have gets an empty scope, so nothing of it is granted.
async fn title_destinations(
    library: &LibraryStore,
    host: &HostContext,
    title: Option<&AcquireTitle>,
    destinations: &[super::uploads::OwnerDestination],
) -> Vec<UploadDestination> {
    let cfg = host.state.config.read().await.clone();
    let mut out = Vec::with_capacity(destinations.len());
    for destination in destinations {
        let (title_folders, title_names) = match title {
            Some(title) => {
                let request = crate::library_link::host_jobs::acquire_request(
                    &cfg,
                    title.into(),
                    &destination.options,
                );
                let planned = bookclerk_acquire::planned_title_names(library, &request).await;
                let names = (!planned.own_folder).then_some(TitleNames {
                    audio_stems: planned.audio_stems,
                    part_patterns: planned.part_patterns,
                });
                (planned.folders, names)
            }
            None => (Vec::new(), None),
        };
        out.push(UploadDestination {
            id: destination.id.clone(),
            title_folders,
            title_names,
        });
    }
    out
}

/// `upload.grant`: opens an upload of one title and seals its token to the
/// host.
pub(super) async fn upload_grant(
    library: &LibraryStore,
    host: &HostContext,
    id: String,
    job_id: String,
    fence: i64,
    request: UploadGrantRequest<'_>,
) -> bookclerk_library::Result<Frame> {
    if request.files.len() > MAX_UPLOAD_FILES {
        return Ok(error(id, "refused", "invalid_manifest"));
    }
    // The manifest must cover exactly the destinations deployed here now
    // (the upload endpoint re-checks them for every file), each only under
    // the title's own keys there, computed here from this node's row.
    let scoped = if request.holding_id.is_empty() {
        Vec::new()
    } else {
        let title = library.acquire_title_of_holding(request.holding_id).await?;
        // Only a title `acquire.plan` would hand out now: one whose audio
        // is already acquired on its edition, or that the plan leaves out
        // otherwise, is never uploaded again, whatever the host asks.
        if let Some(title) = &title {
            let download_episodes = host.state.config.read().await.library.download_episodes;
            if !crate::jobs::acquire_job_includes_title(title, false, download_episodes) {
                if live_remote_job(library, &host.node_id, &job_id, fence)
                    .await?
                    .is_none()
                {
                    return Ok(lost(id, job_id, fence));
                }
                return Ok(error(id, "refused", "title_not_planned"));
            }
        }
        let destinations = match super::uploads::owner_destinations(&host.state).await {
            Ok(destinations) if !destinations.is_empty() => destinations,
            _ => return Ok(error(id, "unavailable", "no_destination")),
        };
        title_destinations(library, host, title.as_ref(), &destinations).await
    };
    let request = UploadGrantRequest {
        destinations: &scoped,
        ..request
    };
    let granted = grant_upload(
        library,
        &host.node_id,
        &job_id,
        fence,
        &host.shared_node_id,
        &request,
    )
    .await?;
    Ok(match granted {
        UploadGrant::Granted {
            upload_id,
            token,
            expires_at,
            superseded,
        } => {
            // A re-sent title replaces its earlier upload; what that one
            // had received is deleted before the new one takes files,
            // except the keys the new one claimed (it writes them again).
            for session in &superseded {
                super::uploads::discard_received(&host.state, library, session).await;
            }
            let mut ephemeral = [0u8; 32];
            rand::rngs::OsRng.fill_bytes(&mut ephemeral);
            let sealed = seal_upload_token(
                &host.box_public_key,
                &ephemeral,
                &host.node_id,
                &job_id,
                fence,
                &upload_id,
                token.as_bytes(),
            );
            ephemeral.zeroize();
            drop(token);
            match sealed {
                Ok(sealed_token) => Frame::new(
                    id,
                    Body::UploadGranted {
                        upload_id,
                        sealed_token,
                        expires_in_secs: u64::try_from(
                            (expires_at - chrono::Utc::now()).num_seconds().max(0),
                        )
                        .unwrap_or(0),
                    },
                ),
                Err(_) => {
                    let _ =
                        bookclerk_library::control_plane::abort_upload(library, &upload_id).await;
                    error(id, "unavailable", "the upload token could not be sealed")
                }
            }
        }
        UploadGrant::Lost => lost(id, job_id, fence),
        UploadGrant::Refused(reason) => {
            tracing::warn!(
                node_id = %host.node_id,
                job_id = %job_id,
                fence,
                reason,
                "refused a host's upload"
            );
            error(id, "refused", reason)
        }
    })
}
