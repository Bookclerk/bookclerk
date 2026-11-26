//! Coverage-guided decoding of the frames a plugin writes on a framed
//! (STARTTLS-capable) tunnel through the launcher's egress relay. The oracle
//! in `fuzz/oracles/egress_starttls_frames.rs` is shared with the replay
//! test.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/egress_starttls_frames.rs"]
mod oracle;

fuzz_target!(|data: &[u8]| {
    oracle::check(data);
});
