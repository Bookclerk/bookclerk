//! Shared oracle for the `node_account_bundle` fuzz target and the
//! `bookclerk-node-proto` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The input is the opened plaintext of an account bundle: the store
//! accounts, with their secrets, that a library sends a joining branch for
//! the person who approved the join. The branch writes each account and
//! secret into its own database, so the invariants are:
//!
//! - parsing never panics, and refuses bytes over the bundle size limit,
//!   another bundle version, and a bundle for another cluster or node;
//! - an accepted bundle carries at most the account and secret limits, each
//!   text field is 1 to its limit in bytes with no control character and no
//!   surrounding whitespace, each secret is 1 to its limit in bytes, and no
//!   account or secret within an account repeats, checked here rather than
//!   through the parser's own rule;
//! - an accepted bundle serializes and parses back to an equal bundle.

use std::collections::BTreeSet;

use bookclerk_node_proto::{
    parse_account_bundle, ACCOUNT_BUNDLE_VERSION, MAX_ACCOUNT_BUNDLE_BYTES, MAX_ACCOUNT_SECRETS,
    MAX_BUNDLE_ACCOUNTS, MAX_CARRIED_ACCOUNT_ID_BYTES, MAX_CARRIED_FIELD_BYTES,
    MAX_CARRIED_PLUGIN_KEY_BYTES, MAX_CARRIED_SECRET_BYTES,
};

/// The cluster the oracle opens bundles for (seeds name it).
pub const CLUSTER_ID: &str = "cluster-1";

/// The node the oracle opens bundles for (seeds name it).
pub const NODE_ID: &str = "node-1";

/// Panics unless `value` is a carried text field of at most `max` bytes.
fn assert_field(what: &str, value: &str, max: usize) {
    assert!(
        !value.is_empty() && value.len() <= max,
        "{what} of {} bytes accepted",
        value.len()
    );
    for ch in value.chars() {
        let code = u32::from(ch);
        assert!(
            code >= 0x20 && !(0x7f..=0x9f).contains(&code),
            "{what} with control character U+{code:04X} accepted"
        );
    }
    let first = value.chars().next();
    let last = value.chars().last();
    assert!(
        !first.is_some_and(char::is_whitespace) && !last.is_some_and(char::is_whitespace),
        "{what} with surrounding whitespace accepted"
    );
}

/// Runs every invariant on one input; panics on a violation.
pub fn check(data: &[u8]) {
    let Ok(bundle) = parse_account_bundle(data, CLUSTER_ID, NODE_ID) else {
        return;
    };
    assert!(
        data.len() <= MAX_ACCOUNT_BUNDLE_BYTES,
        "an oversized bundle parsed"
    );
    assert_eq!(
        bundle.v, ACCOUNT_BUNDLE_VERSION,
        "another bundle version parsed"
    );
    assert_eq!(
        bundle.cluster_id, CLUSTER_ID,
        "a bundle for another cluster parsed"
    );
    assert_eq!(bundle.node_id, NODE_ID, "a bundle for another node parsed");
    assert!(
        bundle.accounts.len() <= MAX_BUNDLE_ACCOUNTS,
        "too many accounts"
    );
    let mut accounts = BTreeSet::new();
    for account in &bundle.accounts {
        assert_field(
            "plugin key",
            &account.plugin_key,
            MAX_CARRIED_PLUGIN_KEY_BYTES,
        );
        assert_field(
            "account id",
            &account.account_id,
            MAX_CARRIED_ACCOUNT_ID_BYTES,
        );
        assert_field("source", &account.source, MAX_CARRIED_FIELD_BYTES);
        assert_field("marketplace", &account.marketplace, MAX_CARRIED_FIELD_BYTES);
        if let Some(label) = &account.label {
            assert_field("label", label, MAX_CARRIED_FIELD_BYTES);
        }
        assert!(
            accounts.insert((account.plugin_key.clone(), account.account_id.clone())),
            "a repeated account was accepted"
        );
        assert!(
            account.secrets.len() <= MAX_ACCOUNT_SECRETS,
            "too many secrets"
        );
        let mut secrets = BTreeSet::new();
        for secret in &account.secrets {
            assert_field("secret kind", &secret.kind, MAX_CARRIED_FIELD_BYTES);
            assert_field("secret name", &secret.name, MAX_CARRIED_ACCOUNT_ID_BYTES);
            let len = secret.value.expose().len();
            assert!(
                (1..=MAX_CARRIED_SECRET_BYTES).contains(&len),
                "a secret of {len} bytes accepted"
            );
            assert!(
                secrets.insert((secret.kind.clone(), secret.name.clone())),
                "a repeated secret was accepted"
            );
        }
    }
    let Ok(text) = serde_json::to_vec(&bundle) else {
        panic!("an accepted bundle did not serialize");
    };
    // The bundle is sent as the library serializes it; a longer form than
    // the limit is never sent, so there is no round trip to check.
    if text.len() > MAX_ACCOUNT_BUNDLE_BYTES {
        return;
    }
    // The error names only the rule; the bundle's secrets stay out of it.
    let again = match parse_account_bundle(&text, CLUSTER_ID, NODE_ID) {
        Ok(again) => again,
        Err(err) => panic!("an accepted bundle did not parse again: {err}"),
    };
    assert!(again == bundle, "a bundle changed across a round trip");
}
