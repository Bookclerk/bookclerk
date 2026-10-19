//! What a host's scan and acquire send back over the node socket (H-4
//! step 9; decisions 4 and 6).
//!
//! | Frame | Answer |
//! |---|---|
//! | `scan.results` | `scan.accepted {batch, upserted}`, `lost`, or `error {code: "refused"}` |
//! | `acquire.plan` | `acquire.targets {targets, destinations}`, `lost`, or `error` |
//! | `upload.grant` | `upload.granted {upload_id, sealed_token, expires_in_secs}`, `lost`, or `error` |
//!
//! A scan's rows are parsed as the guest's `ScanBook` and handed to
//! [`accept_scan_results`], which checks the fence, the node's trust and
//! every row's account on each batch before it writes any row; the host
//! never writes library SQL. An acquire asks for its titles (the owner's
//! pending rows the job covers, with every destination deployed on this
//! node and that destination's own options, so the host names each
//! destination's files as a local acquire would), then for each title asks
//! for an upload whose manifest gives each file's key per destination (each
//! key must lie in the title's own scope there, which this node computes
//! from its own row, and be no other title's file); the
//! token is sealed to the host's box key and bound to the node, job, fence
//! and upload, so it is never frame text.

use bookclerk_library::control_plane::{
    accept_scan_results, grant_upload, job_titles, live_remote_job, ScanAccept, UploadDestination,
    UploadGrant, UploadGrantRequest,
};
use bookclerk_library::{JobKind, LibraryStore};
use bookclerk_node_proto::{
    seal_upload_token, AcquireTarget, Body, Frame, PlanDestination, MAX_FRAME_BYTES,
    MAX_SCAN_BATCH_ROWS, MAX_SCAN_ROW_BYTES, MAX_UPLOAD_FILES,
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
    for row in rows {
        if row.to_string().len() > MAX_SCAN_ROW_BYTES {
            return Ok(error(id, "refused", "row_too_large"));
        }
        let Ok(book) = serde_json::from_value::<bookclerk_plugin_abi::ScanBook>(row) else {
            return Ok(error(id, "refused", "invalid_row"));
        };
        books.push(bookclerk_source::abi::scan_book_to_new("", book));
    }
    Ok(
        match accept_scan_results(library, &host.node_id, &job_id, fence, plugin_key, books).await?
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
    let Some(books) = job_titles(library, &host.node_id, &job).await? else {
        return Ok(lost(id, job_id, fence));
    };
    let mut targets = Vec::new();
    for book in books {
        if targets.len() >= MAX_PLAN_TARGETS {
            break;
        }
        // Only titles whose audio is not acquired, exactly what the closing
        // commit waits for. A title whose audio is acquired but whose
        // companion PDF is missing is left to an acquire on a node holding
        // the library: a host would fetch and upload the audio again, and
        // a missing PDF is a soft failure that never keeps a job open.
        if !crate::jobs::acquire_job_includes_book(&book, false, download_episodes) {
            continue;
        }
        targets.push(AcquireTarget {
            book_uuid: book.uuid.clone(),
            plugin_key: book.plugin_key.clone(),
            account_id: book.account_id.clone(),
            book: serde_json::to_value(&book).unwrap_or_default(),
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

/// This node's destinations with the key scope of the title `book_uuid`
/// on each: the folders a local acquire of the title would write under,
/// named with that destination's own options from this node's library row
/// ([`bookclerk_acquire::planned_title_folders`]). A title this node does
/// not have gets an empty scope, so nothing of it is granted.
///
/// # Errors
///
/// Returns an error when the title cannot be read.
async fn title_destinations(
    library: &LibraryStore,
    host: &HostContext,
    book_uuid: &str,
    destinations: &[super::uploads::OwnerDestination],
) -> bookclerk_library::Result<Vec<UploadDestination>> {
    let book = library.get_book_by_uuid(book_uuid).await?;
    let cfg = host.state.config.read().await.clone();
    let mut out = Vec::with_capacity(destinations.len());
    for destination in destinations {
        let title_folders = match &book {
            Some(book) => {
                let request = crate::cluster_link::host_jobs::acquire_request(
                    &cfg,
                    book,
                    &destination.options,
                );
                bookclerk_acquire::planned_title_folders(library, &request).await
            }
            None => Vec::new(),
        };
        out.push(UploadDestination {
            id: destination.id.clone(),
            title_folders,
        });
    }
    Ok(out)
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
    let scoped = if request.book_uuid.is_empty() {
        Vec::new()
    } else {
        let destinations = match super::uploads::owner_destinations(&host.state).await {
            Ok(destinations) if !destinations.is_empty() => destinations,
            _ => return Ok(error(id, "unavailable", "no_destination")),
        };
        title_destinations(library, host, request.book_uuid, &destinations).await?
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
        &host.owner_node_id,
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
