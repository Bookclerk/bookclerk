//! Coverage-guided validation of the records metadata-file dialects return
//! (`bookclerk-plugin-abi::validate_metadata_file_record` and
//! `check_record_against_dialect`). A record is a jailed guest's
//! `MetadataFiles.parse` answer about a sidecar file the host found on
//! storage, and the host checks it before storing a claim from it. The
//! oracle in `fuzz/oracles/metadata_file_record.rs` is shared with the
//! replay test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/metadata_file_record.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    oracle::check(data);
});
