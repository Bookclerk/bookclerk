//! Shared oracle for the `shared_branch_bootstrap` fuzz target and the
//! `bookclerk-node-proto` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The input is the opened plaintext of a shared branch bootstrap: the
//! library's `[database]` connection, the database plugin's own credential
//! (D1's API token; a Postgres URL is its own) and its data-encryption key,
//! which a joining shared branch writes as its own bootstrap and keeps in
//! its master key store. The invariants are:
//!
//! - parsing never panics, and refuses bytes over the size limit, another
//!   version, and a bootstrap for another library or node;
//! - an accepted bootstrap's DEK is exactly 32 bytes;
//! - an accepted credential secret, and an accepted Postgres URL, is 1 to
//!   its limit in bytes of UTF-8 with no whitespace or control character,
//!   and an accepted credential id is 1 to its limit of ASCII letters,
//!   digits, `-` and `_`, checked here rather than through the parser's own
//!   rule;
//! - a Postgres bootstrap never carries a separate credential;
//! - an accepted bootstrap serializes and parses back to an equal one.

use bookclerk_node_proto::{
    parse_shared_branch_bootstrap, SharedBranchDatabase, DEK_LEN, MAX_BOOTSTRAP_CREDENTIAL_BYTES,
    MAX_BOOTSTRAP_CREDENTIAL_ID_BYTES, MAX_SHARED_BRANCH_BOOTSTRAP_BYTES,
    SHARED_BRANCH_BOOTSTRAP_VERSION,
};

/// The library the oracle opens bootstraps for (seeds name it).
pub const LIBRARY_ID: &str = "library-1";

/// The node the oracle opens bootstraps for (seeds name it).
pub const NODE_ID: &str = "node-1";

/// Runs every invariant on one input; panics on a violation.
pub fn check(data: &[u8]) {
    let Ok(bootstrap) = parse_shared_branch_bootstrap(data, LIBRARY_ID, NODE_ID) else {
        return;
    };
    assert!(
        data.len() <= MAX_SHARED_BRANCH_BOOTSTRAP_BYTES,
        "an oversized bootstrap parsed"
    );
    assert_eq!(
        bootstrap.v, SHARED_BRANCH_BOOTSTRAP_VERSION,
        "another bootstrap version parsed"
    );
    assert_eq!(bootstrap.library_id, LIBRARY_ID, "another library parsed");
    assert_eq!(bootstrap.node_id, NODE_ID, "another node parsed");
    assert_eq!(bootstrap.dek.len(), DEK_LEN, "a short or long DEK parsed");
    match &bootstrap.database {
        SharedBranchDatabase::Postgres { url } => {
            assert!(
                bootstrap.database_credential.is_none(),
                "a Postgres bootstrap with a separate credential parsed"
            );
            check_secret(url.expose(), "Postgres URL");
        }
        SharedBranchDatabase::D1 { .. } => {}
    }
    if let Some(credential) = &bootstrap.database_credential {
        check_secret(credential.secret.expose(), "credential");
        if let Some(id) = &credential.secret_id {
            assert!(
                !id.is_empty() && id.len() <= MAX_BOOTSTRAP_CREDENTIAL_ID_BYTES,
                "a credential id of {} bytes parsed",
                id.len()
            );
            for byte in id.bytes() {
                assert!(
                    byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_',
                    "a credential id with byte {byte:#04x} parsed"
                );
            }
        }
    }
    let Ok(text) = serde_json::to_vec(&bootstrap) else {
        panic!("an accepted bootstrap did not serialize");
    };
    // The library never sends a form longer than the limit.
    if text.len() > MAX_SHARED_BRANCH_BOOTSTRAP_BYTES {
        return;
    }
    // The error names only the rule; the bootstrap's secrets stay out of it.
    let back = match parse_shared_branch_bootstrap(&text, LIBRARY_ID, NODE_ID) {
        Ok(back) => back,
        Err(err) => panic!("an accepted bootstrap did not parse again: {err}"),
    };
    assert!(back == bootstrap, "a bootstrap changed across a round trip");
}

/// Panics unless `raw` is 1 to the limit in bytes of UTF-8 with no
/// whitespace or control character.
fn check_secret(raw: &[u8], what: &str) {
    assert!(
        !raw.is_empty() && raw.len() <= MAX_BOOTSTRAP_CREDENTIAL_BYTES,
        "a {what} of {} bytes parsed",
        raw.len()
    );
    let Ok(text) = std::str::from_utf8(raw) else {
        panic!("a non-UTF-8 {what} parsed");
    };
    for ch in text.chars() {
        assert!(
            !ch.is_whitespace() && !ch.is_control(),
            "a {what} with U+{:04X} parsed",
            u32::from(ch)
        );
    }
}
