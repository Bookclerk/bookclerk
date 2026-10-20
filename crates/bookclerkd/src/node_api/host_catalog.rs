//! What a branch shares with this library over the node socket (branches
//! of branches).
//!
//! | Frame | Answer |
//! |---|---|
//! | `catalog.begin` | `catalog.session {fence, cursor, ancestry, destinations, wanted}` or `error {code: "refused"}` |
//! | `catalog.sync` | `catalog.accepted {batch, upserted, skipped, wanted}`, `lost`, or `error {code: "refused"}` |
//! | `catalog.upload` | `upload.granted`, `lost`, or `error` |
//!
//! The branch is this connection's authenticated node; nothing in a frame
//! names another one. Titles are recorded as its owner user's
//! ([`accept_catalog_batch`]); a catalog upload's token is sealed to the
//! branch's box key, bound to the node, [`CATALOG_ITEM`], the catalog fence
//! and the upload, and its files go through the same upload endpoint as a
//! host acquire's, straight to this node's destinations.

use bookclerk_library::control_plane::{
    accept_catalog_batch, begin_catalog, grant_catalog_upload, library_ancestry, CatalogAccept,
    CatalogBegin, UploadGrant,
};
use bookclerk_library::LibraryStore;
use bookclerk_node_proto::{
    seal_upload_token, Body, CatalogTitle, Frame, UploadFile, CATALOG_ITEM, MAX_UPLOAD_FILES,
};
use rand::RngCore as _;
use zeroize::Zeroize as _;

use super::work::{error, HostContext};

/// A `lost` answer for the catalog fence.
fn lost(id: String, fence: i64) -> Frame {
    Frame::new(
        id,
        Body::Lost {
            item: CATALOG_ITEM.into(),
            job_id: String::new(),
            fence,
        },
    )
}

/// This node's destination ids (the primary first); empty when none is
/// deployed, so the branch shares the catalog but no files.
async fn destination_ids(host: &HostContext) -> Vec<String> {
    match super::uploads::owner_destinations(&host.state).await {
        Ok(destinations) => super::uploads::destination_ids(&destinations),
        Err(_) => Vec::new(),
    }
}

/// `catalog.begin`.
pub(super) async fn begin(
    library: &LibraryStore,
    host: &HostContext,
    id: String,
    share_books: bool,
    include_branches: bool,
) -> bookclerk_library::Result<Frame> {
    match begin_catalog(library, &host.node_id, share_books, include_branches).await? {
        CatalogBegin::Opened(opened) => {
            let destinations = destination_ids(host).await;
            let wanted = if destinations.is_empty() {
                Vec::new()
            } else {
                opened.wanted
            };
            Ok(Frame::new(
                id,
                Body::CatalogSession {
                    fence: opened.fence,
                    cursor: opened.cursor,
                    ancestry: library_ancestry(library).await?,
                    destinations,
                    wanted,
                },
            ))
        }
        CatalogBegin::Refused(reason) => Ok(error(id, "refused", reason)),
    }
}

/// One `catalog.sync` batch as the branch sent it.
pub(super) struct CatalogBatch {
    /// The catalog fence.
    pub fence: i64,
    /// The branch's batch counter, echoed.
    pub batch: u64,
    /// Cursor after the batch.
    pub cursor: String,
    /// Last batch of the pass.
    pub done: bool,
    /// The titles.
    pub titles: Vec<CatalogTitle>,
}

/// `catalog.sync`.
pub(super) async fn sync(
    library: &LibraryStore,
    host: &HostContext,
    id: String,
    batch: CatalogBatch,
) -> bookclerk_library::Result<Frame> {
    let CatalogBatch {
        fence,
        batch,
        cursor,
        done,
        titles,
    } = batch;
    let count = titles.len();
    Ok(
        match accept_catalog_batch(library, &host.node_id, fence, &cursor, done, &titles).await? {
            CatalogAccept::Accepted {
                upserted,
                skipped,
                wanted,
            } => {
                tracing::info!(
                    node_id = %host.node_id,
                    fence,
                    batch,
                    titles = count,
                    upserted,
                    skipped,
                    "recorded a branch's shared titles"
                );
                let wanted = if destination_ids(host).await.is_empty() {
                    Vec::new()
                } else {
                    wanted
                };
                Frame::new(
                    id,
                    Body::CatalogAccepted {
                        batch,
                        upserted,
                        skipped,
                        wanted,
                    },
                )
            }
            CatalogAccept::Lost => lost(id, fence),
            CatalogAccept::Refused(reason) => {
                tracing::warn!(node_id = %host.node_id, fence, batch, reason, "refused a branch's shared titles");
                error(id, "refused", reason)
            }
        },
    )
}

/// `catalog.upload`: opens an upload of one shared title and seals its
/// token to the branch.
pub(super) async fn upload(
    library: &LibraryStore,
    host: &HostContext,
    id: String,
    fence: i64,
    title: (String, String),
    files: Vec<UploadFile>,
) -> bookclerk_library::Result<Frame> {
    if files.len() > MAX_UPLOAD_FILES {
        return Ok(error(id, "refused", "invalid_manifest"));
    }
    let destinations = destination_ids(host).await;
    if destinations.is_empty() {
        return Ok(error(id, "unavailable", "no_destination"));
    }
    let (store_id, product_id) = title;
    let granted = grant_catalog_upload(
        library,
        &host.node_id,
        fence,
        &host.owner_node_id,
        &store_id,
        &product_id,
        &files,
        &destinations,
    )
    .await?;
    Ok(match granted {
        UploadGrant::Granted {
            upload_id,
            token,
            expires_at,
            superseded,
        } => {
            // A re-sent title replaces its earlier upload, as for a job's
            // upload: what that one had received is deleted first, except
            // the keys the new one claimed.
            for session in &superseded {
                super::uploads::discard_received(&host.state, library, session).await;
            }
            let mut ephemeral = [0u8; 32];
            rand::rngs::OsRng.fill_bytes(&mut ephemeral);
            let sealed = seal_upload_token(
                &host.box_public_key,
                &ephemeral,
                &host.node_id,
                CATALOG_ITEM,
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
        UploadGrant::Lost => lost(id, fence),
        UploadGrant::Refused(reason) => {
            tracing::warn!(node_id = %host.node_id, fence, reason, "refused a branch's catalog upload");
            error(id, "refused", reason)
        }
    })
}
