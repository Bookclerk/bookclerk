//! Types storefront guests share with the host.
//!
//! # Audience
//!
//! Native storefront guests (Libro.fm, Chirp, GraphicAudio, …) and the host
//! crates that talk to them. A guest reaches the library only through the
//! plugin ABI, so this crate carries the storefront DTOs, the conversions to
//! and from the typed ABI ([`abi`]), the listing / record helpers and the
//! text normalization both sides apply, and nothing that opens a database.
//!
//! It deliberately has no dependency on `bookclerk-library`,
//! `bookclerk-config`, an ORM, a SQL driver or OpenAPI, so a guest build
//! stays small. The host-only half (the `ContentSource` trait, the source
//! registry, `DownloadOptions`, `FetchOptions` and the
//! conversions to library rows) lives in `bookclerk-source`, which
//! re-exports the storefront DTOs, ABI conversions and listing / record
//! helpers. Host code gets the text and content-kind helpers from
//! `bookclerk-library`, which re-exports them from here.
//!
//! Product narrative: `docs/plugins/README.md`. Style: `docs/contributing/code-documentation.md`.

pub mod abi;
mod brand;
pub mod content_kind;
mod error;
mod language;
pub mod listing;
mod media;
pub mod records;
pub mod text;
mod types;

pub use brand::SourceBrand;
pub use content_kind::{is_episode, is_podcast_parent};
pub use error::{Result, SourceError};
pub use language::{default_preferred_language, language_rank, normalize_language};
pub use media::{
    audio_extension, extension_from_bytes, extension_from_content_type, extension_from_url,
};
pub use text::{
    decode_html_entities, decode_html_entities_cow, decode_html_entities_in_place,
    decode_html_entities_opt, decode_html_entities_opt_in_place, str_maybe_html_entity,
};
pub use types::{
    CatalogHit, CatalogSearchField, CatalogSearchOpts, CatalogSearchSort, CatalogSortDir,
    ConfigOptionValue, ExpandSeed, ImportCredentialsOptions, LoginOptions, OAuthProgress,
    PlainAudioPart, PlainFetch, PurchaseHintOpts, ScanOptions, ScanSummary, SourceAccount,
    SourceConfigOption, SourceFetch, SourcePurchaseHint,
};
