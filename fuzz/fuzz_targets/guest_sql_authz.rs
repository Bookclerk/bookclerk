//! Coverage-guided guest SQL authorization (`bookclerk-plugin-abi`
//! `GuestSqlPolicy`): the gate between plugin SQL and the host database.
//!
//! The first input byte selects a host-issued policy (lexical allowlist,
//! column-restricted, typed catalog, plugin-owned binding, binding migration)
//! and the bind type; the rest is one canonical SQL statement. A statement the
//! host admits is lowered as the SQLite adapter lowers it and compiled on an
//! in-memory database under SQLite's own authorizer callback, an independent
//! view of every table, column and function the statement touches. Each such
//! access must be inside the policy (`docs/adr/untrusted-input-fuzzing.md`).
//! The replay test in `bookclerk-library/tests/fuzz_corpus.rs` shares the
//! oracle in `fuzz/oracles/guest_sql_authz.rs`.

#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../oracles/guest_sql_authz.rs"]
#[allow(dead_code)]
mod oracle;

fuzz_target!(|data: &[u8]| {
    let _ = oracle::check(data);
});
