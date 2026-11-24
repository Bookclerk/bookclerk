//! Joining a library as a **shared branch**: a shared node (`kind = shared`) that
//! shares the library's database and data-encryption key (DEK), as opposed
//! to a **branch** (a branch node with its own database).
//!
//! The connection type is chosen on the library's consent page and bound to
//! the authorization code there: a branch code is granted [`BRANCH_SCOPE`],
//! a shared branch code [`SHARED_BRANCH_GRANTED_SCOPE`] (the library stores either with the
//! consented branch name after it), and the redeem names the type it
//! expects ([`ConnectionType`]), so a code approved for a branch can never
//! be redeemed as a shared branch (or the other way round).
//!
//! A shared branch's redeem answer carries a [`SharedBranchBootstrap`] sealed to the
//! joining node's X25519 box key with the H-4 sealed box
//! ([`crate::seal`]: X25519, HKDF-SHA256, XChaCha20-Poly1305), bound to the
//! library and node ids ([`shared_branch_bootstrap_aad`]). It holds the library's
//! `[database]` connection, the database plugin's own credential when the
//! connection settings don't carry it ([`SharedBranchCredential`]: D1's API
//! token), and the DEK itself, never the passphrase-wrapped `master.key` file. Only the
//! daemon that generated the box key can open it; the browser never sees
//! it. [`parse_shared_branch_bootstrap`] reads the opened plaintext, sans
//! IO, and is what the `shared_branch_bootstrap` fuzz target drives.

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::join::BRANCH_SCOPE;
use crate::keys::PUBLIC_KEY_LEN;
use crate::sealed::{open, seal, SealError, SecretValue};

/// The scope a shared branch code is granted (stored with the code at consent).
pub const SHARED_BRANCH_GRANTED_SCOPE: &str = "node:enroll node:shared_branch";

/// Authorize-request parameter by which a joining install offers to join as
/// a shared branch (`shared_branch_offer=1`): it is empty, so its own data may be replaced.
/// Without it the consent page offers a branch only.
pub const SHARED_BRANCH_OFFER_PARAM: &str = "shared_branch_offer";

/// Domain tag that starts the associated data of a shared branch bootstrap.
pub const SHARED_BRANCH_BOOTSTRAP_AAD_DOMAIN: &[u8] = b"bookclerk-node.v1 shared-branch-bootstrap";

/// Version of [`SharedBranchBootstrap`].
pub const SHARED_BRANCH_BOOTSTRAP_VERSION: u32 = 1;

/// Length of the data-encryption key.
pub const DEK_LEN: usize = 32;

/// The longest opened bootstrap accepted, in bytes.
pub const MAX_SHARED_BRANCH_BOOTSTRAP_BYTES: usize = 64 * 1024;

/// The longest database credential (a Postgres URL, a D1 API token) a
/// bootstrap may carry, in bytes.
pub const MAX_BOOTSTRAP_CREDENTIAL_BYTES: usize = 4096;

/// The longest provider id for a database credential a bootstrap may carry,
/// in bytes.
pub const MAX_BOOTSTRAP_CREDENTIAL_ID_BYTES: usize = 128;

/// How a joining install connects to a library.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionType {
    /// A branch node: its own database, work over the node socket (default).
    #[default]
    Branch,
    /// A shared node: shares the library's database and DEK.
    SharedBranch,
}

impl ConnectionType {
    /// Stable text (`branch`, `shared_branch`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Branch => "branch",
            Self::SharedBranch => "shared_branch",
        }
    }

    /// Parses `branch` or `shared_branch` (case-insensitive); anything else is
    /// `None`.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "branch" => Some(Self::Branch),
            "shared_branch" => Some(Self::SharedBranch),
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
            Self::SharedBranch => SHARED_BRANCH_GRANTED_SCOPE,
        }
    }
}

/// The library's database, as a shared branch needs it to connect.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "plugin", rename_all = "lowercase")]
pub enum SharedBranchDatabase {
    /// PostgreSQL: the connection URL (it carries credentials).
    Postgres {
        /// `postgres://user:password@host/db`.
        url: SecretValue,
    },
    /// Cloudflare D1: the database's ids. Its API token travels as the
    /// bootstrap's [`SharedBranchCredential`].
    D1 {
        /// Cloudflare account id.
        account_id: String,
        /// D1 database UUID.
        database_id: String,
        /// Cloudflare API base.
        api_base: String,
    },
}

impl SharedBranchDatabase {
    /// The `[database].plugin` id (`postgres`, `d1`).
    #[must_use]
    pub fn plugin(&self) -> &'static str {
        match self {
            Self::Postgres { .. } => "postgres",
            Self::D1 { .. } => "d1",
        }
    }
}

impl std::fmt::Debug for SharedBranchDatabase {
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

/// The database plugin's own credential, for a plugin whose connection
/// settings don't carry it (D1's API token; a Postgres URL carries its
/// own). Whatever the plugin, the shared branch keeps it sealed with the DEK
/// in its master key store (file, OS keychain or `systemd-creds`), tagged
/// with the plugin it belongs to. `Debug` prints no secret.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SharedBranchCredential {
    /// The secret: 1 to [`MAX_BOOTSTRAP_CREDENTIAL_BYTES`] bytes of UTF-8
    /// with no whitespace or control character.
    pub secret: SecretValue,
    /// The provider's id for the secret, when the guided setup minted it
    /// (so a later setup on the shared branch can replace it): 1 to
    /// [`MAX_BOOTSTRAP_CREDENTIAL_ID_BYTES`] ASCII letters, digits, `-` and
    /// `_`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_id: Option<String>,
}

impl std::fmt::Debug for SharedBranchCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedBranchCredential")
            .field("secret", &"<redacted>")
            .field("secret_id", &self.secret_id)
            .finish()
    }
}

/// What a shared branch receives at redeem: the `[database]` bootstrap and the DEK,
/// for the node id the library recorded (`kind = shared`). `Debug` prints no
/// secret.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SharedBranchBootstrap {
    /// [`SHARED_BRANCH_BOOTSTRAP_VERSION`].
    pub v: u32,
    /// The library id.
    pub library_id: String,
    /// The node id recorded for this shared branch.
    pub node_id: String,
    /// How to reach the library's database.
    pub database: SharedBranchDatabase,
    /// The database plugin's own credential, when `database` doesn't carry
    /// it. Never sent with a Postgres `database`, whose URL is its
    /// credential; left out when the library has none to give (a D1
    /// library whose token is configured only where it runs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database_credential: Option<SharedBranchCredential>,
    /// The library's 32-byte data-encryption key.
    pub dek: SecretValue,
}

impl std::fmt::Debug for SharedBranchBootstrap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedBranchBootstrap")
            .field("v", &self.v)
            .field("library_id", &self.library_id)
            .field("node_id", &self.node_id)
            .field("database", &self.database)
            .field("database_credential", &self.database_credential)
            .finish_non_exhaustive()
    }
}

/// Associated data of a shared branch bootstrap for `node_id` of `library_id`: the
/// domain tag, then each field as a 4-byte big-endian length and its bytes.
#[must_use]
pub fn shared_branch_bootstrap_aad(library_id: &str, node_id: &str) -> Vec<u8> {
    let mut out = SHARED_BRANCH_BOOTSTRAP_AAD_DOMAIN.to_vec();
    for field in [library_id.as_bytes(), node_id.as_bytes()] {
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
pub fn seal_shared_branch_bootstrap(
    recipient: &[u8],
    ephemeral_secret: &[u8; PUBLIC_KEY_LEN],
    bootstrap: &SharedBranchBootstrap,
) -> Result<String, SealError> {
    if bootstrap.v != SHARED_BRANCH_BOOTSTRAP_VERSION || bootstrap.dek.len() != DEK_LEN {
        return Err(SealError::Bundle);
    }
    let plaintext = Zeroizing::new(serde_json::to_vec(bootstrap).map_err(|_| SealError::Bundle)?);
    let sealed = seal(
        recipient,
        ephemeral_secret,
        &shared_branch_bootstrap_aad(&bootstrap.library_id, &bootstrap.node_id),
        &plaintext,
    )?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sealed))
}

/// Opens a bootstrap made by [`seal_shared_branch_bootstrap`] with the joining
/// node's box secret, for the library and node ids the redeem answered.
///
/// # Errors
///
/// Returns [`SealError::Open`] when the box was sealed to another key, for
/// another library or node, or was altered, and [`SealError::Bundle`] when
/// its content is malformed or names other ids.
pub fn open_shared_branch_bootstrap(
    recipient_secret: &[u8; PUBLIC_KEY_LEN],
    library_id: &str,
    node_id: &str,
    sealed: &str,
) -> Result<SharedBranchBootstrap, SealError> {
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(sealed.trim().as_bytes())
        .map_err(|_| SealError::Open)?;
    let plaintext = open(
        recipient_secret,
        &shared_branch_bootstrap_aad(library_id, node_id),
        &raw,
    )?;
    parse_shared_branch_bootstrap(&plaintext, library_id, node_id)
}

/// Reads an opened bootstrap for `library_id` and `node_id`.
///
/// # Arguments
///
/// * `plaintext` - the bootstrap JSON, as [`open_shared_branch_bootstrap`]
///   opened it.
/// * `library_id`, `node_id` - the ids the redeem answered.
///
/// # Errors
///
/// [`SealError::Bundle`] for bytes over [`MAX_SHARED_BRANCH_BOOTSTRAP_BYTES`],
/// bytes that are not a bootstrap, another version, a DEK that is not 32
/// bytes, other ids, a Postgres URL or credential secret that is empty,
/// longer than [`MAX_BOOTSTRAP_CREDENTIAL_BYTES`] or holds anything but
/// UTF-8 without whitespace or control characters, a credential id longer than
/// [`MAX_BOOTSTRAP_CREDENTIAL_ID_BYTES`] or holding anything but ASCII
/// letters, digits, `-` and `_`, or a credential sent with a Postgres
/// database.
pub fn parse_shared_branch_bootstrap(
    plaintext: &[u8],
    library_id: &str,
    node_id: &str,
) -> Result<SharedBranchBootstrap, SealError> {
    if plaintext.len() > MAX_SHARED_BRANCH_BOOTSTRAP_BYTES {
        return Err(SealError::Bundle);
    }
    let bootstrap: SharedBranchBootstrap =
        serde_json::from_slice(plaintext).map_err(|_| SealError::Bundle)?;
    if bootstrap.v != SHARED_BRANCH_BOOTSTRAP_VERSION
        || bootstrap.dek.len() != DEK_LEN
        || bootstrap.library_id != library_id
        || bootstrap.node_id != node_id
    {
        return Err(SealError::Bundle);
    }
    match &bootstrap.database {
        SharedBranchDatabase::Postgres { url } => {
            if bootstrap.database_credential.is_some() || !valid_credential_secret(url.expose()) {
                return Err(SealError::Bundle);
            }
        }
        SharedBranchDatabase::D1 { .. } => {}
    }
    if let Some(credential) = &bootstrap.database_credential {
        let id_ok = credential
            .secret_id
            .as_deref()
            .is_none_or(valid_credential_id);
        if !valid_credential_secret(credential.secret.expose()) || !id_ok {
            return Err(SealError::Bundle);
        }
    }
    Ok(bootstrap)
}

/// True for a credential a shared branch can store: 1 to
/// [`MAX_BOOTSTRAP_CREDENTIAL_BYTES`] bytes of UTF-8 with no whitespace or
/// control character.
fn valid_credential_secret(secret: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(secret) else {
        return false;
    };
    !text.is_empty()
        && text.len() <= MAX_BOOTSTRAP_CREDENTIAL_BYTES
        && !text.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// True for a credential id of 1 to [`MAX_BOOTSTRAP_CREDENTIAL_ID_BYTES`]
/// ASCII letters, digits, `-` and `_`.
fn valid_credential_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_BOOTSTRAP_CREDENTIAL_ID_BYTES
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::box_public_key;

    fn bootstrap() -> SharedBranchBootstrap {
        SharedBranchBootstrap {
            v: SHARED_BRANCH_BOOTSTRAP_VERSION,
            library_id: "library-1".into(),
            node_id: "node-1".into(),
            database: SharedBranchDatabase::Postgres {
                url: SecretValue::new(b"postgres://u:hunter2@db/lib".to_vec()),
            },
            database_credential: None,
            dek: SecretValue::new(vec![7u8; DEK_LEN]),
        }
    }

    #[test]
    fn a_bootstrap_opens_only_with_the_node_key_and_its_ids() {
        let node_secret = [3u8; 32];
        let other_secret = [4u8; 32];
        let sealed =
            seal_shared_branch_bootstrap(&box_public_key(&node_secret), &[9u8; 32], &bootstrap())
                .unwrap();
        assert!(!sealed.contains("hunter2"));
        let opened =
            open_shared_branch_bootstrap(&node_secret, "library-1", "node-1", &sealed).unwrap();
        assert_eq!(opened, bootstrap());
        // Another key, another node or library id, or a flipped byte: no.
        for (secret, library, node) in [
            (other_secret, "library-1", "node-1"),
            (node_secret, "library-2", "node-1"),
            (node_secret, "library-1", "node-2"),
        ] {
            assert_eq!(
                open_shared_branch_bootstrap(&secret, library, node, &sealed),
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
            open_shared_branch_bootstrap(&node_secret, "library-1", "node-1", &tampered),
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
            seal_shared_branch_bootstrap(&box_public_key(&[3u8; 32]), &[9u8; 32], &short),
            Err(SealError::Bundle)
        );
    }

    /// A D1 bootstrap carries the library's token as the database
    /// credential; a bad secret or id, or a credential beside a Postgres
    /// URL, is refused.
    #[test]
    fn a_bootstrap_carries_a_checked_database_credential() {
        let d1 = |secret: Option<&[u8]>, id: Option<&str>| SharedBranchBootstrap {
            database: SharedBranchDatabase::D1 {
                account_id: "acct".into(),
                database_id: "db".into(),
                api_base: "https://api.cloudflare.com/client/v4".into(),
            },
            database_credential: secret.map(|s| SharedBranchCredential {
                secret: SecretValue::new(s.to_vec()),
                secret_id: id.map(str::to_string),
            }),
            ..bootstrap()
        };
        let parse = |b: &SharedBranchBootstrap| {
            parse_shared_branch_bootstrap(&serde_json::to_vec(b).unwrap(), "library-1", "node-1")
        };
        let good = d1(Some(b"d1-token"), Some("tok_1"));
        assert_eq!(parse(&good).unwrap(), good);
        let printed = format!("{good:?}");
        assert!(!printed.contains("d1-token"), "{printed}");
        // A library with no token to give sends none.
        assert!(parse(&d1(None, None)).is_ok());
        assert!(parse(&bootstrap()).is_ok());
        let mut postgres_with_credential = bootstrap();
        postgres_with_credential.database_credential = good.database_credential.clone();
        let mut postgres_bad_url = bootstrap();
        postgres_bad_url.database = SharedBranchDatabase::Postgres {
            url: SecretValue::new(b"postgres://u:p@db/lib two".to_vec()),
        };
        for bad in [
            d1(Some(b""), None),
            d1(Some(b"has space"), None),
            d1(Some(&[0xff, 0xfe]), None),
            d1(Some(&[b'x'; MAX_BOOTSTRAP_CREDENTIAL_BYTES + 1]), None),
            d1(Some(b"tok"), Some("bad id")),
            d1(Some(b"tok"), Some("")),
            postgres_with_credential,
            postgres_bad_url,
        ] {
            assert_eq!(parse(&bad), Err(SealError::Bundle), "{bad:?}");
        }
        let huge = vec![b' '; MAX_SHARED_BRANCH_BOOTSTRAP_BYTES + 1];
        assert_eq!(
            parse_shared_branch_bootstrap(&huge, "library-1", "node-1"),
            Err(SealError::Bundle)
        );
    }

    #[test]
    fn connection_types_bind_their_scope() {
        assert_eq!(ConnectionType::default(), ConnectionType::Branch);
        assert_eq!(ConnectionType::Branch.granted_scope(), BRANCH_SCOPE);
        assert_eq!(
            ConnectionType::SharedBranch.granted_scope(),
            SHARED_BRANCH_GRANTED_SCOPE
        );
        assert_ne!(
            ConnectionType::Branch.granted_scope(),
            ConnectionType::SharedBranch.granted_scope()
        );
        assert_eq!(
            ConnectionType::parse(" Shared_Branch "),
            Some(ConnectionType::SharedBranch)
        );
        assert_eq!(ConnectionType::parse("peer"), None);
    }
}
