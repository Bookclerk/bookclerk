//! SeaORM entities mirroring the greenfield schema in [`crate::migrations`].
//!
//! One `DeriveEntityModel` per table. Column Rust types match the proxy/native
//! backends: integer columns are `i64`, reals `f64`, blobs `Vec<u8>`, and text
//! (including RFC 3339 timestamps) `String`. Timestamps stay as `String` to
//! match the `TEXT` columns without relying on SeaORM's proxy chrono decoding;
//! [`crate::store`] parses them into `chrono::DateTime<Utc>` at the record
//! boundary. [`LibraryStore`](crate::LibraryStore) uses these entities
//! (`Entity::find`, `ActiveModel`) for the majority of CRUD.

pub mod account_links;
pub mod accounts;
pub mod books;
pub mod claim_tickets;
pub mod embeddings;
pub mod encrypted_secrets;
pub mod ignored_titles;
pub mod listening_progress;
pub mod oidc_auth_codes;
pub mod oidc_clients;
pub mod oidc_refresh_tokens;
pub mod operator_sessions;
pub mod portal_identities;
pub mod portal_sessions;
pub mod saved_filters;
pub mod security_audit_events;
pub mod title_request_sources;
pub mod title_requests;
pub mod user_invites;
pub mod user_preferences;
pub mod users;
pub mod work_editions;
pub mod works;
