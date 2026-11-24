//! Coverage-guided parsing of the bootstrap a library sends a joining
//! shared branch (`bookclerk-node-proto::parse_shared_branch_bootstrap`):
//! its `[database]` connection, the D1 API token and the data-encryption
//! key, which the shared branch writes as its own. The oracle in
//! `fuzz/oracles/shared_branch_bootstrap.rs` is shared with the replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/shared_branch_bootstrap.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    oracle::check(data);
});
