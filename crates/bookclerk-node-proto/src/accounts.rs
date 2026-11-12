//! Store accounts that follow their person to a branch ("Accounts on every
//! instance" in the library catalog network ADR).
//!
//! When a library's user lets a new branch join and ticks "Bring my store
//! connections to this branch", the library seals that user's own store
//! accounts, with their secrets, to the joining node's box key and returns
//! them in the redeem answer ([`crate::OAuthEnrollAnswer::accounts`]). Only
//! accounts linked to the signing-in user travel, never another member's,
//! and only those whose storefront declares `credentials = "shareable"`.
//!
//! The box is bound to the library's cluster id and the node id it minted
//! ([`account_bundle_aad`]), so it cannot be replayed into another join.
//! The branch opens it with [`open_account_bundle`], which runs
//! [`parse_account_bundle`]: a sans-IO check over the opened bytes that
//! the `node_account_bundle` fuzz target drives too. A bundle that names
//! other ids, breaks a size limit, repeats an account or a secret, or
//! carries an empty or oversized value is refused whole.
//!
//! Secret values are [`SecretValue`]s: zeroized on drop and redacted in
//! `Debug`.

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::keys::PUBLIC_KEY_LEN;
use crate::sealed::{open, seal, SealError, SecretValue};

/// Version of an [`AccountBundle`].
pub const ACCOUNT_BUNDLE_VERSION: u32 = 1;

/// Domain tag that starts the associated data of an account bundle.
pub const ACCOUNT_BUNDLE_AAD_DOMAIN: &[u8] = b"bookclerk-node.v1 account-bundle";

/// Most accounts one bundle carries.
pub const MAX_BUNDLE_ACCOUNTS: usize = 64;

/// Most secrets one carried account holds.
pub const MAX_ACCOUNT_SECRETS: usize = 16;

/// Largest secret value carried, in bytes.
pub const MAX_CARRIED_SECRET_BYTES: usize = 64 * 1024;

/// Largest opened bundle accepted, in bytes. A bundle at every limit above
/// would be far larger; this keeps the redeem answer small.
pub const MAX_ACCOUNT_BUNDLE_BYTES: usize = 512 * 1024;

/// Longest plugin key carried, in bytes.
pub const MAX_CARRIED_PLUGIN_KEY_BYTES: usize = 256;

/// Longest store account id carried, in bytes.
pub const MAX_CARRIED_ACCOUNT_ID_BYTES: usize = 256;

/// Longest source alias, marketplace, secret kind or label carried, in
/// bytes.
pub const MAX_CARRIED_FIELD_BYTES: usize = 128;

/// The store accounts a library hands a joining branch, for one person.
/// `Debug` prints no secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountBundle {
    /// [`ACCOUNT_BUNDLE_VERSION`].
    pub v: u32,
    /// The library's cluster id.
    pub cluster_id: String,
    /// The node id the library minted for the branch.
    pub node_id: String,
    /// The person's accounts, each with its secrets.
    pub accounts: Vec<CarriedAccount>,
}

/// One store account in an [`AccountBundle`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CarriedAccount {
    /// Provenance-qualified plugin key of the storefront that owns the
    /// account.
    pub plugin_key: String,
    /// Store account id, unique within its plugin key.
    pub account_id: String,
    /// Display alias of the storefront (`libro`, `chirp`, ...).
    pub source: String,
    /// Store marketplace or locale code (`us`, `uk`, ...).
    pub marketplace: String,
    /// The account's display label, when it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// The account's sealed secrets, unsealed for the box.
    pub secrets: Vec<CarriedSecret>,
}

/// One secret of a [`CarriedAccount`]: an `encrypted_secrets` row of the
/// account, by kind and name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CarriedSecret {
    /// Secret kind (`source_auth`, ...).
    pub kind: String,
    /// Logical secret name within the account.
    pub name: String,
    /// The secret bytes.
    pub value: SecretValue,
}

/// Associated data of the account bundle for `node_id` of `cluster_id`:
/// the domain tag, then each field as a 4-byte big-endian length and its
/// bytes.
#[must_use]
pub fn account_bundle_aad(cluster_id: &str, node_id: &str) -> Vec<u8> {
    let mut out = ACCOUNT_BUNDLE_AAD_DOMAIN.to_vec();
    for field in [cluster_id.as_bytes(), node_id.as_bytes()] {
        out.extend_from_slice(&u32::try_from(field.len()).unwrap_or(u32::MAX).to_be_bytes());
        out.extend_from_slice(field);
    }
    out
}

/// True for a carried text field: 1 to `max` bytes, no control characters
/// and no surrounding whitespace.
fn is_field(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value.trim() == value
        && !value.chars().any(char::is_control)
}

/// Checks `bundle` against every rule [`parse_account_bundle`] applies.
///
/// # Errors
///
/// Returns [`SealError::Bundle`] for a wrong version or ids, too many
/// accounts or secrets, an empty or oversized field or value, a repeated
/// account, or a repeated secret within an account.
pub fn check_account_bundle(
    bundle: &AccountBundle,
    cluster_id: &str,
    node_id: &str,
) -> Result<(), SealError> {
    if bundle.v != ACCOUNT_BUNDLE_VERSION
        || bundle.cluster_id != cluster_id
        || bundle.node_id != node_id
        || bundle.accounts.len() > MAX_BUNDLE_ACCOUNTS
    {
        return Err(SealError::Bundle);
    }
    let mut seen = std::collections::BTreeSet::new();
    for account in &bundle.accounts {
        let fields_ok = is_field(&account.plugin_key, MAX_CARRIED_PLUGIN_KEY_BYTES)
            && is_field(&account.account_id, MAX_CARRIED_ACCOUNT_ID_BYTES)
            && is_field(&account.source, MAX_CARRIED_FIELD_BYTES)
            && is_field(&account.marketplace, MAX_CARRIED_FIELD_BYTES)
            && account
                .label
                .as_deref()
                .is_none_or(|label| is_field(label, MAX_CARRIED_FIELD_BYTES));
        if !fields_ok
            || account.secrets.len() > MAX_ACCOUNT_SECRETS
            || !seen.insert((account.plugin_key.as_str(), account.account_id.as_str()))
        {
            return Err(SealError::Bundle);
        }
        let mut names = std::collections::BTreeSet::new();
        for secret in &account.secrets {
            if !is_field(&secret.kind, MAX_CARRIED_FIELD_BYTES)
                || !is_field(&secret.name, MAX_CARRIED_ACCOUNT_ID_BYTES)
                || secret.value.is_empty()
                || secret.value.len() > MAX_CARRIED_SECRET_BYTES
                || !names.insert((secret.kind.as_str(), secret.name.as_str()))
            {
                return Err(SealError::Bundle);
            }
        }
    }
    Ok(())
}

/// Parses the opened bytes of an account bundle for `node_id` of
/// `cluster_id` and checks it ([`check_account_bundle`]). Sans-IO: the
/// branch runs it on what [`open`] returns, and the fuzz target runs it on
/// arbitrary bytes.
///
/// # Errors
///
/// Returns [`SealError::Bundle`] for bytes over
/// [`MAX_ACCOUNT_BUNDLE_BYTES`], bytes that are not a bundle, and a bundle
/// the check refuses.
pub fn parse_account_bundle(
    plaintext: &[u8],
    cluster_id: &str,
    node_id: &str,
) -> Result<AccountBundle, SealError> {
    if plaintext.len() > MAX_ACCOUNT_BUNDLE_BYTES {
        return Err(SealError::Bundle);
    }
    let bundle: AccountBundle = serde_json::from_slice(plaintext).map_err(|_| SealError::Bundle)?;
    check_account_bundle(&bundle, cluster_id, node_id)?;
    Ok(bundle)
}

/// Seals `bundle` to the joining node's box key `recipient`, as base64url
/// text for the redeem answer.
///
/// # Errors
///
/// Returns [`SealError::Key`] for an invalid recipient key and
/// [`SealError::Bundle`] for a bundle [`check_account_bundle`] refuses for
/// its own ids, or whose JSON is over [`MAX_ACCOUNT_BUNDLE_BYTES`].
pub fn seal_account_bundle(
    recipient: &[u8],
    ephemeral_secret: &[u8; PUBLIC_KEY_LEN],
    bundle: &AccountBundle,
) -> Result<String, SealError> {
    check_account_bundle(bundle, &bundle.cluster_id, &bundle.node_id)?;
    let plaintext = Zeroizing::new(serde_json::to_vec(bundle).map_err(|_| SealError::Bundle)?);
    if plaintext.len() > MAX_ACCOUNT_BUNDLE_BYTES {
        return Err(SealError::Bundle);
    }
    let sealed = seal(
        recipient,
        ephemeral_secret,
        &account_bundle_aad(&bundle.cluster_id, &bundle.node_id),
        &plaintext,
    )?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sealed))
}

/// Opens a bundle made by [`seal_account_bundle`] with the joining node's
/// box secret, for the cluster and node ids the redeem answered.
///
/// # Errors
///
/// Returns [`SealError::Open`] when the box was sealed to another key, for
/// another cluster or node, or was altered, and [`SealError::Bundle`] when
/// its content is refused by [`parse_account_bundle`].
pub fn open_account_bundle(
    recipient_secret: &[u8; PUBLIC_KEY_LEN],
    cluster_id: &str,
    node_id: &str,
    sealed: &str,
) -> Result<AccountBundle, SealError> {
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(sealed.trim().as_bytes())
        .map_err(|_| SealError::Open)?;
    let plaintext = open(
        recipient_secret,
        &account_bundle_aad(cluster_id, node_id),
        &raw,
    )?;
    parse_account_bundle(&plaintext, cluster_id, node_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::box_public_key;

    /// A bundle for `node-1` of `cluster-1` with one account.
    fn bundle() -> AccountBundle {
        AccountBundle {
            v: ACCOUNT_BUNDLE_VERSION,
            cluster_id: "cluster-1".into(),
            node_id: "node-1".into(),
            accounts: vec![CarriedAccount {
                plugin_key: "platform:bookclerk/bookclerk-plugin-source-libro#libro".into(),
                account_id: "reader@example.com".into(),
                source: "libro".into(),
                marketplace: "us".into(),
                label: Some("Reader".into()),
                secrets: vec![CarriedSecret {
                    kind: "source_auth".into(),
                    name: "reader.libro".into(),
                    value: SecretValue::new(b"{\"token\":\"t\"}".to_vec()),
                }],
            }],
        }
    }

    /// A fixed recipient secret and its public key.
    fn recipient() -> ([u8; 32], Vec<u8>) {
        let secret = [7u8; 32];
        (secret, box_public_key(&secret).to_vec())
    }

    #[test]
    fn a_bundle_opens_only_for_its_cluster_and_node() {
        let (secret, public) = recipient();
        let sealed = seal_account_bundle(&public, &[9u8; 32], &bundle()).unwrap();
        assert_eq!(
            open_account_bundle(&secret, "cluster-1", "node-1", &sealed).unwrap(),
            bundle()
        );
        assert_eq!(
            open_account_bundle(&secret, "cluster-2", "node-1", &sealed),
            Err(SealError::Open)
        );
        assert_eq!(
            open_account_bundle(&secret, "cluster-1", "node-2", &sealed),
            Err(SealError::Open)
        );
        assert_eq!(
            open_account_bundle(&[8u8; 32], "cluster-1", "node-1", &sealed),
            Err(SealError::Open)
        );
    }

    #[test]
    fn debug_never_prints_a_secret_value() {
        let shown = format!("{:?}", bundle());
        assert!(!shown.contains("token"), "{shown}");
        assert!(shown.contains("reader@example.com"), "{shown}");
    }

    #[test]
    fn the_check_refuses_every_broken_rule() {
        let refused = |edit: &dyn Fn(&mut AccountBundle)| {
            let mut b = bundle();
            edit(&mut b);
            check_account_bundle(&b, "cluster-1", "node-1")
        };
        assert_eq!(refused(&|_| {}), Ok(()));
        assert!(refused(&|b| b.v = 2).is_err());
        assert!(refused(&|b| b.node_id = "node-2".into()).is_err());
        assert!(refused(&|b| b.accounts[0].plugin_key = String::new()).is_err());
        assert!(refused(&|b| b.accounts[0].account_id = " padded".into()).is_err());
        assert!(refused(&|b| b.accounts[0].label = Some("a\u{7}b".into())).is_err());
        assert!(refused(&|b| b.accounts[0].source = "s".repeat(129)).is_err());
        assert!(refused(&|b| {
            let again = b.accounts[0].clone();
            b.accounts.push(again);
        })
        .is_err());
        assert!(refused(&|b| {
            let again = b.accounts[0].secrets[0].clone();
            b.accounts[0].secrets.push(again);
        })
        .is_err());
        assert!(
            refused(&|b| b.accounts[0].secrets[0].value = SecretValue::new(Vec::new())).is_err()
        );
        assert!(refused(&|b| {
            b.accounts[0].secrets[0].value =
                SecretValue::new(vec![1; MAX_CARRIED_SECRET_BYTES + 1]);
        })
        .is_err());
        assert!(refused(&|b| {
            let one = b.accounts[0].clone();
            b.accounts = (0..=MAX_BUNDLE_ACCOUNTS)
                .map(|n| CarriedAccount {
                    account_id: format!("a{n}"),
                    ..one.clone()
                })
                .collect();
        })
        .is_err());
    }

    #[test]
    fn parse_refuses_oversized_and_unknown_shapes() {
        assert_eq!(
            parse_account_bundle(&vec![b' '; MAX_ACCOUNT_BUNDLE_BYTES + 1], "c", "n"),
            Err(SealError::Bundle)
        );
        assert_eq!(
            parse_account_bundle(b"{\"v\":1}", "c", "n"),
            Err(SealError::Bundle)
        );
        let mut text = serde_json::to_value(bundle()).unwrap();
        text["extra"] = serde_json::json!(true);
        assert_eq!(
            parse_account_bundle(&serde_json::to_vec(&text).unwrap(), "cluster-1", "node-1"),
            Err(SealError::Bundle)
        );
    }

    #[test]
    fn sealing_refuses_a_bundle_the_check_refuses() {
        let (_, public) = recipient();
        let mut b = bundle();
        b.v = 0;
        assert_eq!(
            seal_account_bundle(&public, &[9u8; 32], &b),
            Err(SealError::Bundle)
        );
    }
}
