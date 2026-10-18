//! Joining a library as a **clerk**: an owner node (`kind = owner`) that
//! shares the library's database and data-encryption key (DEK), as opposed
//! to a **branch** (a host node with its own database).
//!
//! The connection type is chosen on the library's consent page and bound to
//! the authorization code there: a branch code is granted [`BRANCH_SCOPE`],
//! a clerk code [`CLERK_GRANTED_SCOPE`], and the redeem names the type it
//! expects ([`ConnectionType`]), so a code approved for a branch can never
//! be redeemed as a clerk (or the other way round).
//!
//! A clerk's redeem answer carries a [`ClerkBootstrap`] sealed to the
//! joining node's X25519 box key with the H-4 sealed box
//! ([`crate::seal`]: X25519, HKDF-SHA256, XChaCha20-Poly1305), bound to the
//! cluster and node ids ([`clerk_bootstrap_aad`]). It holds the library's
//! `[database]` connection and the DEK itself, never the passphrase-wrapped
//! `master.key` file. Only the daemon that generated the box key can open
//! it; the browser never sees it.

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::join::BRANCH_SCOPE;
use crate::keys::PUBLIC_KEY_LEN;
use crate::sealed::{open, seal, SealError, SecretValue};

/// The scope a clerk code is granted (stored with the code at consent).
pub const CLERK_GRANTED_SCOPE: &str = "node:enroll node:clerk";

/// Authorize-request parameter by which a joining install offers to join as
/// a clerk (`clerk_offer=1`): it is empty, so its own data may be replaced.
/// Without it the consent page offers a branch only.
pub const CLERK_OFFER_PARAM: &str = "clerk_offer";

/// Domain tag that starts the associated data of a clerk bootstrap.
pub const CLERK_BOOTSTRAP_AAD_DOMAIN: &[u8] = b"bookclerk-node.v1 clerk-bootstrap";

/// Version of [`ClerkBootstrap`].
pub const CLERK_BOOTSTRAP_VERSION: u32 = 1;

/// Length of the data-encryption key.
pub const DEK_LEN: usize = 32;

/// How a joining install connects to a library.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConnectionType {
    /// A host node: its own database, work over the node socket (default).
    #[default]
    Branch,
    /// An owner node: shares the library's database and DEK.
    Clerk,
}

impl ConnectionType {
    /// Stable text (`branch`, `clerk`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Branch => "branch",
            Self::Clerk => "clerk",
        }
    }

    /// Parses `branch` or `clerk` (case-insensitive); anything else is
    /// `None`.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "branch" => Some(Self::Branch),
            "clerk" => Some(Self::Clerk),
            _ => None,
        }
    }

    /// True for [`Self::Branch`] (serde: the field is left out).
    #[must_use]
    pub fn is_branch(&self) -> bool {
        *self == Self::Branch
    }

    /// The scope a code for this connection type is granted.
    #[must_use]
    pub fn granted_scope(self) -> &'static str {
        match self {
            Self::Branch => BRANCH_SCOPE,
            Self::Clerk => CLERK_GRANTED_SCOPE,
        }
    }
}

/// The library's database, as a clerk needs it to connect.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "plugin", rename_all = "lowercase")]
pub enum ClerkDatabase {
    /// PostgreSQL: the connection URL (it carries credentials).
    Postgres {
        /// `postgres://user:password@host/db`.
        url: SecretValue,
    },
    /// Cloudflare D1: the database's ids. The API token stays
    /// environment-only bootstrap on each node and is never sent.
    D1 {
        /// Cloudflare account id.
        account_id: String,
        /// D1 database UUID.
        database_id: String,
        /// Cloudflare API base.
        api_base: String,
    },
}

impl ClerkDatabase {
    /// The `[database].plugin` id (`postgres`, `d1`).
    #[must_use]
    pub fn plugin(&self) -> &'static str {
        match self {
            Self::Postgres { .. } => "postgres",
            Self::D1 { .. } => "d1",
        }
    }
}

impl std::fmt::Debug for ClerkDatabase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Postgres { .. } => f
                .debug_struct("Postgres")
                .field("url", &"<redacted>")
                .finish(),
            Self::D1 {
                account_id,
                database_id,
                api_base,
            } => f
                .debug_struct("D1")
                .field("account_id", account_id)
                .field("database_id", database_id)
                .field("api_base", api_base)
                .finish(),
        }
    }
}

/// What a clerk receives at redeem: the `[database]` bootstrap and the DEK,
/// for the node id the library recorded (`kind = owner`). `Debug` prints no
/// secret.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClerkBootstrap {
    /// [`CLERK_BOOTSTRAP_VERSION`].
    pub v: u32,
    /// The library's cluster id.
    pub cluster_id: String,
    /// The node id recorded for this clerk.
    pub node_id: String,
    /// How to reach the library's database.
    pub database: ClerkDatabase,
    /// The library's 32-byte data-encryption key.
    pub dek: SecretValue,
}

impl std::fmt::Debug for ClerkBootstrap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClerkBootstrap")
            .field("v", &self.v)
            .field("cluster_id", &self.cluster_id)
            .field("node_id", &self.node_id)
            .field("database", &self.database)
            .finish_non_exhaustive()
    }
}

/// Associated data of a clerk bootstrap for `node_id` of `cluster_id`: the
/// domain tag, then each field as a 4-byte big-endian length and its bytes.
#[must_use]
pub fn clerk_bootstrap_aad(cluster_id: &str, node_id: &str) -> Vec<u8> {
    let mut out = CLERK_BOOTSTRAP_AAD_DOMAIN.to_vec();
    for field in [cluster_id.as_bytes(), node_id.as_bytes()] {
        out.extend_from_slice(&u32::try_from(field.len()).unwrap_or(u32::MAX).to_be_bytes());
        out.extend_from_slice(field);
    }
    out
}

/// Seals `bootstrap` to the joining node's box key `recipient`, as
/// base64url text for the redeem answer.
///
/// # Errors
///
/// Returns [`SealError::Key`] for an invalid recipient key and
/// [`SealError::Bundle`] for a bootstrap that is not version 1 or whose DEK
/// is not 32 bytes.
pub fn seal_clerk_bootstrap(
    recipient: &[u8],
    ephemeral_secret: &[u8; PUBLIC_KEY_LEN],
    bootstrap: &ClerkBootstrap,
) -> Result<String, SealError> {
    if bootstrap.v != CLERK_BOOTSTRAP_VERSION || bootstrap.dek.len() != DEK_LEN {
        return Err(SealError::Bundle);
    }
    let plaintext = Zeroizing::new(serde_json::to_vec(bootstrap).map_err(|_| SealError::Bundle)?);
    let sealed = seal(
        recipient,
        ephemeral_secret,
        &clerk_bootstrap_aad(&bootstrap.cluster_id, &bootstrap.node_id),
        &plaintext,
    )?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sealed))
}

/// Opens a bootstrap made by [`seal_clerk_bootstrap`] with the joining
/// node's box secret, for the cluster and node ids the redeem answered.
///
/// # Errors
///
/// Returns [`SealError::Open`] when the box was sealed to another key, for
/// another cluster or node, or was altered, and [`SealError::Bundle`] when
/// its content is malformed or names other ids.
pub fn open_clerk_bootstrap(
    recipient_secret: &[u8; PUBLIC_KEY_LEN],
    cluster_id: &str,
    node_id: &str,
    sealed: &str,
) -> Result<ClerkBootstrap, SealError> {
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(sealed.trim().as_bytes())
        .map_err(|_| SealError::Open)?;
    let plaintext = open(
        recipient_secret,
        &clerk_bootstrap_aad(cluster_id, node_id),
        &raw,
    )?;
    let bootstrap: ClerkBootstrap =
        serde_json::from_slice(&plaintext).map_err(|_| SealError::Bundle)?;
    if bootstrap.v != CLERK_BOOTSTRAP_VERSION
        || bootstrap.dek.len() != DEK_LEN
        || bootstrap.cluster_id != cluster_id
        || bootstrap.node_id != node_id
    {
        return Err(SealError::Bundle);
    }
    Ok(bootstrap)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::box_public_key;

    fn bootstrap() -> ClerkBootstrap {
        ClerkBootstrap {
            v: CLERK_BOOTSTRAP_VERSION,
            cluster_id: "cluster-1".into(),
            node_id: "node-1".into(),
            database: ClerkDatabase::Postgres {
                url: SecretValue::new(b"postgres://u:hunter2@db/lib".to_vec()),
            },
            dek: SecretValue::new(vec![7u8; DEK_LEN]),
        }
    }

    #[test]
    fn a_bootstrap_opens_only_with_the_node_key_and_its_ids() {
        let node_secret = [3u8; 32];
        let other_secret = [4u8; 32];
        let sealed =
            seal_clerk_bootstrap(&box_public_key(&node_secret), &[9u8; 32], &bootstrap()).unwrap();
        assert!(!sealed.contains("hunter2"));
        let opened = open_clerk_bootstrap(&node_secret, "cluster-1", "node-1", &sealed).unwrap();
        assert_eq!(opened, bootstrap());
        // Another key, another node or cluster id, or a flipped byte: no.
        for (secret, cluster, node) in [
            (other_secret, "cluster-1", "node-1"),
            (node_secret, "cluster-2", "node-1"),
            (node_secret, "cluster-1", "node-2"),
        ] {
            assert_eq!(
                open_clerk_bootstrap(&secret, cluster, node, &sealed),
                Err(SealError::Open)
            );
        }
        let mut raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(&sealed)
            .unwrap();
        let last = raw.len() - 1;
        raw[last] ^= 1;
        let tampered = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw);
        assert_eq!(
            open_clerk_bootstrap(&node_secret, "cluster-1", "node-1", &tampered),
            Err(SealError::Open)
        );
    }

    #[test]
    fn bootstraps_never_print_their_secrets_and_need_a_full_dek() {
        let printed = format!("{:?}", bootstrap());
        assert!(!printed.contains("hunter2"), "{printed}");
        assert!(!printed.contains("7, 7"), "{printed}");
        let mut short = bootstrap();
        short.dek = SecretValue::new(vec![1u8; 16]);
        assert_eq!(
            seal_clerk_bootstrap(&box_public_key(&[3u8; 32]), &[9u8; 32], &short),
            Err(SealError::Bundle)
        );
    }

    #[test]
    fn connection_types_bind_their_scope() {
        assert_eq!(ConnectionType::default(), ConnectionType::Branch);
        assert_eq!(ConnectionType::Branch.granted_scope(), BRANCH_SCOPE);
        assert_eq!(ConnectionType::Clerk.granted_scope(), CLERK_GRANTED_SCOPE);
        assert_ne!(
            ConnectionType::Branch.granted_scope(),
            ConnectionType::Clerk.granted_scope()
        );
        assert_eq!(
            ConnectionType::parse(" Clerk "),
            Some(ConnectionType::Clerk)
        );
        assert_eq!(ConnectionType::parse("peer"), None);
    }
}
