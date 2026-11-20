//! Negative author-surface harness.
//!
//! `scripts/check-author-surface.sh` copies one `cases/*.rs` file over
//! `src/case.rs` and requires `cargo check` to fail. The committed
//! placeholder keeps the harness compiling as a control.
#![allow(unused_imports)]

// Most cases name only the abi crate, so the placeholder control would
// otherwise leave the SDK unused and Cargo would warn about it (#960).
use bookclerk_plugin_sdk as _;

mod case;
