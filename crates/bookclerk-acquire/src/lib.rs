//! Acquire orchestration: license → download → decrypt → metadata → storage.

mod convert;
mod cue;
mod destinations;
mod error;
mod match_storage;
mod naming;
mod pipeline;
mod reconcile;
mod split;

pub use convert::{convert_book, ConvertRequest, ConvertSummary};
pub use destinations::{AcquireDestination, AcquireDestinations};
pub use error::{AcquireError, Result};
pub use match_storage::{match_storage_to_library, MatchStorageOptions, MatchStorageSummary};
pub use naming::{
    audio_basename, chapter_storage_key, chapter_storage_key_with_folder, default_storage_key,
    resolve_templates, sidecar_key, storage_key, storage_key_with_contexts, storage_key_with_rules,
    swap_audio_extension, NamingContext,
};
pub use pipeline::{
    acquire_book, acquire_book_indexed, acquire_pdf_only, planned_storage_key,
    planned_storage_key_for, planned_storage_key_with_rules, AcquireRequest, AcquireResult,
};
pub use reconcile::{
    extract_asins_from_key, find_existing_for_book, find_existing_for_request, reconcile_library,
    ReconcileOptions, ReconcileSummary, StorageIndex,
};
