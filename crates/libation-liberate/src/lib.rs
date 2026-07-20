//! Liberate orchestration: license → download → decrypt → metadata → storage.

mod convert;
mod cue;
mod error;
mod naming;
mod pipeline;
mod reconcile;
mod split;

pub use convert::{convert_book, ConvertRequest, ConvertSummary};
pub use error::{LiberateError, Result};
pub use naming::{
    audio_basename, chapter_storage_key, chapter_storage_key_with_folder, default_storage_key,
    sidecar_key, storage_key, storage_key_with_contexts, storage_key_with_rules,
    swap_audio_extension, NamingContext,
};
pub use pipeline::{
    liberate_book, liberate_book_indexed, liberate_pdf_only, planned_storage_key,
    planned_storage_key_for, planned_storage_key_with_rules, LiberateRequest, LiberateResult,
};
pub use reconcile::{
    extract_asins_from_key, find_existing_for_book, find_existing_for_request, reconcile_library,
    ReconcileOptions, ReconcileSummary, StorageIndex,
};
