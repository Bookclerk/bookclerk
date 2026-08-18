//! Library-sync request pins shared with the CLI engine.
//!
//! The SQLite-backed sync engine compiles only with the `cli` feature.
//! Bookclerk's Audible plugin uses [`DEFAULT_RESPONSE_GROUPS`] when calling
//! the library API directly.

/// Response groups every sync request uses; pinned per database
/// (reference branch default).
pub const DEFAULT_RESPONSE_GROUPS: &str = "badge_types,is_archived,is_finished,is_playable,is_removable,is_visible,\
     order_details,origin_asin,percent_complete,shared,ws4v_rights,badges,\
     category_ladders,category_media,category_metadata,contributors,customer_rights,\
     media,product_attrs,product_desc,product_details,product_extended_attrs,\
     product_plans,product_plan_details,profile_sharing,rating,relationships_v2,\
     sample,sku,pdf_url,series";

#[cfg(feature = "cli")]
#[path = "library_sync_engine.rs"]
mod engine;
#[cfg(feature = "cli")]
pub use engine::*;
