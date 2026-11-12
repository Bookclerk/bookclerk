//! Coverage-guided parsing of the account bundle a library sends a joining
//! branch (`bookclerk-node-proto::parse_account_bundle`): the person's
//! store accounts and their secrets, which the branch writes into its own
//! database. The oracle in `fuzz/oracles/node_account_bundle.rs` is shared
//! with the replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/node_account_bundle.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    oracle::check(data);
});
