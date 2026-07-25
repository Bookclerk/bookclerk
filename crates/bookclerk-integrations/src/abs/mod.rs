//! Audiobookshelf adapter modules.

mod brand;
mod client;
mod integration;

pub use brand::{brand_for_id, matches_id, BRAND};
pub use client::{AbsApiClient, AbsLibrary, AbsUser};
pub use integration::AbsIntegration;
