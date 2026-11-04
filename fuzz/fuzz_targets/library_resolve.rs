//! Coverage-guided resolution of store listings into works, expressions and
//! editions (`bookclerk-resolve`). Listings come from storefront plugins, so
//! their identifiers, credits and kinds are attacker-shaped.
//!
//! Oracles (`bookclerk_resolve::fuzzing::run_case`): one strong identifier
//! never maps to two editions; a weak identifier alone never joins editions
//! of different media or work kinds or incompatible expressions; strong
//! identifiers naming different editions are never merged and the conflict
//! is recorded; a matched work has the listing's kind (and an episode's
//! parent show); resolving the same input twice changes nothing.

#![no_main]

use libfuzzer_sys::fuzz_target;

/// Largest input worth decoding; `run_case` reads at most 64 listings.
const MAX_INPUT_BYTES: usize = 8 * 1024;

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT_BYTES {
        return;
    }
    if let Err(broken) = bookclerk_resolve::fuzzing::run_case(data) {
        panic!("resolver invariant broken: {broken}");
    }
});
