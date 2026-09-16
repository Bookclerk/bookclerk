//! Negative author-surface harness.
//!
//! `scripts/check-author-surface.sh` copies one `cases/*.rs` file over
//! `src/case.rs` and requires `cargo check` to fail. The committed
//! placeholder keeps the harness compiling as a control.
#![allow(unused_imports)]

mod case;
