//! Coverage-guided fuzzing of an upload `PUT`'s `Content-Range`
//! (`bookclerk-node-proto::parse_upload_range`): the header a host sends
//! with each part of a file larger than one request, which decides which
//! bytes of the declared file the body is, or that the request only asks
//! where the owner is. The oracle in
//! `fuzz/oracles/upload_part.rs` is shared with the crate's corpus replay
//! test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/upload_part.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    let _ = oracle::check(data);
});
