//! Paged storage scan persisted in `storage_scan_rows`.
//!
//! Pages are inserted, then the job checkpoint cursor advances. A crash replays
//! the uncheckpointed page (inserts are idempotent) and does not skip it.
//! Destructive library updates happen only after phase `apply` is checkpointed.
//! The checkpoint records storage [`StorageBackend::instance_id`]. A different
//! instance fails closed. Local indexes are node-local scratch; the cursor is
//! a key, not a portable file offset.

#![allow(clippy::missing_docs_in_private_items)]

use bookclerk_library::{JobFence, LibraryStore};
use bookclerk_plugin_abi::{JobCheckpoint, MAX_CHECKPOINT_BYTES};
use bookclerk_storage::{is_audio_key, StorageBackend, StorageError};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::error::{AcquireError, Result};
use crate::reconcile::{extract_asins_from_key, media_rank, StorageIndex};

const SCAN_VERSION: u32 = 1;

/// Versioned scan progress stored in the job payload checkpoint.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ScanCheckpoint {
    /// Schema version. Unknown versions fail closed.
    pub v: u32,
    /// Always `storage_scan`.
    pub op: String,
    /// [`StorageBackend::instance_id`] this cursor belongs to.
    pub instance_id: String,
    /// List prefix (empty scans the namespace).
    pub namespace: String,
    /// `storage_scan_rows.scan_id`.
    pub generation: String,
    /// `list` while paging, `apply` after the scan committed.
    pub phase: String,
    /// Backend list cursor. `None` starts at the first page.
    pub cursor: Option<String>,
    /// True when the list index or scratch is node-local.
    pub node_local: bool,
}

/// Builds or resumes a durable identity/object index.
///
/// # Errors
///
/// Returns [`AcquireError`] when listing, the database, or a checkpoint write fails,
/// or when `prior` names a different storage instance.
pub async fn scan_storage(
    library: &LibraryStore,
    storage: &dyn StorageBackend,
    fence: Option<&JobFence>,
    prior: Option<&JobCheckpoint>,
    probe_metadata: bool,
) -> Result<StorageIndex> {
    let instance_id = storage.instance_id();
    let node_local = instance_id.starts_with("local:");
    let mut state = match prior {
        Some(raw) => decode_prior(raw, &instance_id)?,
        None => fresh_checkpoint(&instance_id, node_local),
    };
    if state.phase == "apply" {
        return Ok(StorageIndex::with_scan(state.generation));
    }
    let mut restarted = false;
    loop {
        match page_scan(library, storage, fence, &mut state, probe_metadata).await {
            Ok(()) => break,
            Err(AcquireError::Storage(StorageError::InvalidCursor(_))) if !restarted => {
                library.storage_scan_delete(&state.generation).await?;
                state = fresh_checkpoint(&instance_id, node_local);
                restarted = true;
                persist(library, fence, &state).await?;
            }
            Err(err) => return Err(err),
        }
    }
    state.phase = "apply".into();
    state.cursor = None;
    persist(library, fence, &state).await?;
    Ok(StorageIndex::with_scan(state.generation))
}

fn next_scan_id() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    format!(
        "scan-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

fn fresh_checkpoint(instance_id: &str, node_local: bool) -> ScanCheckpoint {
    ScanCheckpoint {
        v: SCAN_VERSION,
        op: "storage_scan".into(),
        instance_id: instance_id.to_string(),
        namespace: String::new(),
        generation: next_scan_id(),
        phase: "list".into(),
        cursor: None,
        node_local,
    }
}

fn decode_prior(raw: &JobCheckpoint, instance_id: &str) -> Result<ScanCheckpoint> {
    if raw.schema_version != SCAN_VERSION {
        return Err(AcquireError::Other(anyhow::anyhow!(
            "unsupported storage checkpoint version {}",
            raw.schema_version
        )));
    }
    let state: ScanCheckpoint = serde_json::from_str(&raw.json)
        .map_err(|err| AcquireError::Other(anyhow::anyhow!("storage checkpoint json: {err}")))?;
    if state.v != SCAN_VERSION || state.op != "storage_scan" {
        return Err(AcquireError::Other(anyhow::anyhow!(
            "storage checkpoint is not a storage_scan v{SCAN_VERSION}"
        )));
    }
    if state.instance_id != instance_id {
        return Err(AcquireError::Other(anyhow::anyhow!(
            "storage checkpoint is bound to `{}`, not `{instance_id}`",
            state.instance_id
        )));
    }
    Ok(state)
}

async fn page_scan(
    library: &LibraryStore,
    storage: &dyn StorageBackend,
    fence: Option<&JobFence>,
    state: &mut ScanCheckpoint,
    probe_metadata: bool,
) -> Result<()> {
    let mut hops = 0u32;
    loop {
        hops = hops.saturating_add(1);
        if hops > 1_000_000 {
            return Err(AcquireError::Storage(StorageError::InvalidCursor(
                "storage scan made no progress".into(),
            )));
        }
        let page = storage
            .list_page(&state.namespace, state.cursor.as_deref(), 0)
            .await?;
        for obj in &page.objects {
            if is_hidden_scan_key(&obj.key) {
                continue;
            }
            let audio = is_audio_key(&obj.key);
            let rank = media_rank(&obj.key);
            library
                .storage_scan_put_object(&state.generation, &obj.key, obj.size, rank, audio)
                .await?;
            if !audio {
                continue;
            }
            let mut ids = extract_asins_from_key(&obj.key);
            if probe_metadata {
                if let Ok(probe) = storage.probe(&obj.key).await {
                    if let Some(asin) = probe.meta.asin {
                        ids.push(asin.to_ascii_uppercase());
                    }
                }
            }
            ids.sort();
            ids.dedup();
            for id in ids {
                library
                    .storage_scan_put_identity(&state.generation, &id, &obj.key, obj.size, rank)
                    .await?;
            }
        }
        match page.next_cursor {
            Some(next) if state.cursor.as_deref() == Some(next.as_str()) => {
                return Err(AcquireError::Storage(StorageError::InvalidCursor(
                    "storage scan cursor did not advance".into(),
                )));
            }
            Some(next) => {
                state.cursor = Some(next);
                persist(library, fence, state).await?;
            }
            None => break,
        }
    }
    Ok(())
}

async fn persist(
    library: &LibraryStore,
    fence: Option<&JobFence>,
    state: &ScanCheckpoint,
) -> Result<()> {
    let Some(fence) = fence else {
        return Ok(());
    };
    let json = serde_json::to_string(state)
        .map_err(|err| AcquireError::Other(anyhow::anyhow!("storage checkpoint: {err}")))?;
    if json.len() > MAX_CHECKPOINT_BYTES as usize {
        return Err(AcquireError::Other(anyhow::anyhow!(
            "storage checkpoint is {} bytes",
            json.len()
        )));
    }
    let checkpoint = JobCheckpoint {
        schema_version: SCAN_VERSION,
        json,
    };
    let wrote = library.checkpoint_running_job(fence, &checkpoint).await?;
    if !wrote {
        return Err(AcquireError::Other(anyhow::anyhow!(
            "lost the job fence while checkpointing the storage scan"
        )));
    }
    Ok(())
}

fn is_hidden_scan_key(key: &str) -> bool {
    key.split('/').any(|part| {
        part == ".bookclerk-stage"
            || part == ".bookclerk-list-index"
            || part == ".bookclerk-orphans"
            || (part.starts_with('.') && part.contains(".bookclerk-tmp-"))
    })
}
