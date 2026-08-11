//! Local filesystem destination plugin shared constants.
//!
//! The platform `local` guest writes acquire output under
//! `[output.local]` / `BOOKCLERK_OUTPUT_LOCAL_ROOT`. See
//! [`guest`] for the Workers RPC storage helpers the host invokes.

pub mod guest;

/// Plugin id string advertised in handshake and config (`[output.local]`).
pub const ID: &str = "local";
