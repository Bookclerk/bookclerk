//! Audiobookshelf adapter modules.

mod brand;
mod client;
mod integration;
mod listening;

pub use brand::{brand_for_id, matches_id, BRAND};
pub use client::{
    AbsApiClient, AbsLibrary, AbsLibraryItem, AbsMediaProgress, AbsUser, AbsUserDetail,
};
pub use integration::AbsIntegration;
pub use listening::sync_listening_progress;
