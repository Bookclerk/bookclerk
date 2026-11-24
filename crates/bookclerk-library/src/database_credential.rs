//! A hosted database's API credential, sealed with the DEK and kept where
//! the master key is.
//!
//! A hosted database (Cloudflare D1) needs an API token before the library
//! can open, so it can't live in `encrypted_secrets` like a store login.
//! `bookclerk config database setup d1` mints one and keeps it here instead of in
//! an environment variable:
//!
//! 1. The token is sealed with the data-encryption key (XChaCha20-Poly1305),
//!    so its bytes are never stored in the clear anywhere.
//! 2. The sealed blob goes in the same store as the master key: in
//!    `database.credential` itself on a file install, or in an OS keychain
//!    item or a `systemd-creds` credential on a keychain or TPM install,
//!    with `database.credential` then holding only a pointer to it.
//!
//! | Header | `database.credential` holds |
//! | --- | --- |
//! | `BCD1` | DEK fingerprint, nonce, ciphertext |
//! | `BCD3` | DEK fingerprint, OS keychain item id |
//! | `BCD4` | DEK fingerprint, `systemd-creds` item id |
//!
//! The keychain and `systemd-creds` items are of
//! [`ItemKind::DatabaseCredential`], under their own service and credential
//! names, so they are never confused with the master key's. Reading fails
//! closed: a missing item or a credential sealed under another key is an
//! error, never an empty token.
//!
//! The file and the items are written by this install only, under the same
//! protection as `master.key` itself, so they are not an untrusted-input
//! boundary (see `docs/adr/untrusted-input-fuzzing.md`).

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, RwLock};

use bookclerk_config::KeyStoreKind;
use bookclerk_keystore::{ItemKind, KeychainItemId, OsKeychain, SystemdCredsVault, Vault};
use zeroize::Zeroizing;

use crate::error::{LibraryError, Result};
use crate::master_key::{
    inspect_master_key, seal_with_dek, unseal_with_dek, MasterKey, SystemdCredsFactory,
};

/// The credential file beside `master.key`.
pub const DATABASE_CREDENTIAL_FILE_NAME: &str = "database.credential";

/// Header of a credential sealed in the file itself.
const MAGIC_FILE: &[u8; 4] = b"BCD1";
/// Header of a pointer to an OS keychain item.
const MAGIC_KEYCHAIN: &[u8; 4] = b"BCD3";
/// Header of a pointer to a `systemd-creds` credential.
const MAGIC_CREDS: &[u8; 4] = b"BCD4";
/// Length of a DEK fingerprint (SHA-256).
const FINGERPRINT_LEN: usize = 32;
/// XChaCha20-Poly1305 nonce length.
const NONCE_LEN: usize = 24;
/// Length of a keychain item id.
const ITEM_LEN: usize = 16;
/// The longest token accepted, in bytes. Cloudflare tokens are about 40.
const MAX_TOKEN_LEN: usize = 1024;
/// The longest token id accepted, in bytes.
const MAX_TOKEN_ID_LEN: usize = 128;
/// Version of the sealed payload.
const PAYLOAD_VERSION: u8 = 1;

/// A hosted database's API token, and the provider's id for it.
///
/// The id lets a later setup run revoke the token it replaces.
#[derive(Clone)]
pub struct DatabaseCredential {
    /// The bearer token; zeroized on drop.
    token: Zeroizing<String>,
    /// The provider's id for the token, when it reported one.
    token_id: Option<String>,
}

impl DatabaseCredential {
    /// A credential from a token and the provider's id for it.
    ///
    /// # Arguments
    ///
    /// * `token` - the bearer token; surrounding whitespace is trimmed.
    /// * `token_id` - the provider's id for the token, if known.
    ///
    /// # Errors
    ///
    /// [`LibraryError::Invalid`] when the token is empty, longer than
    /// 1024 bytes or holds whitespace or control characters, or the id is
    /// longer than 128 bytes or holds anything but ASCII letters, digits,
    /// `-` and `_`.
    pub fn new(token: &str, token_id: Option<&str>) -> Result<Self> {
        let token = token.trim();
        if token.is_empty() || token.len() > MAX_TOKEN_LEN {
            return Err(LibraryError::Invalid(format!(
                "a database API token is 1 to {MAX_TOKEN_LEN} bytes"
            )));
        }
        if token.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(LibraryError::Invalid(
                "a database API token holds no spaces or control characters".into(),
            ));
        }
        let token_id = token_id.map(str::trim).filter(|id| !id.is_empty());
        if let Some(id) = token_id {
            let valid = id.len() <= MAX_TOKEN_ID_LEN
                && id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
            if !valid {
                return Err(LibraryError::Invalid(format!(
                    "a database API token id is up to {MAX_TOKEN_ID_LEN} letters, digits, `-` or `_`"
                )));
            }
        }
        Ok(Self {
            token: Zeroizing::new(token.to_string()),
            token_id: token_id.map(str::to_string),
        })
    }

    /// The bearer token.
    #[must_use]
    pub fn token(&self) -> &str {
        &self.token
    }

    /// The provider's id for the token, when known.
    #[must_use]
    pub fn token_id(&self) -> Option<&str> {
        self.token_id.as_deref()
    }
}

impl std::fmt::Debug for DatabaseCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DatabaseCredential")
            .field("token", &"***")
            .field("token_id", &self.token_id)
            .finish()
    }
}

/// The sealed payload's JSON shape.
#[derive(serde::Serialize, serde::Deserialize)]
struct Payload {
    /// Always [`PAYLOAD_VERSION`].
    v: u8,
    /// The bearer token.
    token: String,
    /// The provider's id for the token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    token_id: Option<String>,
}

/// Which store holds the sealed blob.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Home {
    /// `database.credential` itself.
    File,
    /// An OS keychain item.
    Keychain(KeychainItemId),
    /// A `systemd-creds` credential beside the file.
    SystemdCreds(KeychainItemId),
}

impl Home {
    /// The key store this is.
    fn kind(self) -> KeyStoreKind {
        match self {
            Self::File => KeyStoreKind::File,
            Self::Keychain(_) => KeyStoreKind::Keychain,
            Self::SystemdCreds(_) => KeyStoreKind::SystemdCreds,
        }
    }

    /// How the home is named in messages.
    fn describe(self) -> String {
        match self {
            Self::File => DATABASE_CREDENTIAL_FILE_NAME.to_string(),
            Self::Keychain(item) => format!("OS keychain item {item}"),
            Self::SystemdCreds(item) => format!("systemd-creds credential {item}"),
        }
    }
}

/// The process-wide keychain for credential items.
static KEYCHAIN: OnceLock<RwLock<Arc<dyn Vault>>> = OnceLock::new();
/// The process-wide `systemd-creds` builder for credential items.
static CREDS: OnceLock<RwLock<Arc<SystemdCredsFactory>>> = OnceLock::new();

/// The keychain slot, initialised to the OS keychain.
fn keychain_slot() -> &'static RwLock<Arc<dyn Vault>> {
    KEYCHAIN
        .get_or_init(|| RwLock::new(Arc::new(OsKeychain::for_kind(ItemKind::DatabaseCredential))))
}

/// The `systemd-creds` slot, initialised to the system tool.
fn creds_slot() -> &'static RwLock<Arc<SystemdCredsFactory>> {
    CREDS.get_or_init(|| {
        RwLock::new(Arc::new(|dir: &Path| -> Arc<dyn Vault> {
            Arc::new(SystemdCredsVault::new(dir).for_kind(ItemKind::DatabaseCredential))
        }))
    })
}

/// Replaces the stores credential items go to, for tests (a
/// [`bookclerk_keystore::MemoryVault`]) and hosts that substitute their own.
///
/// # Arguments
///
/// * `keychain` - the keychain credential items are kept in.
/// * `creds` - builds the `systemd-creds` store for a files directory.
pub fn set_database_credential_vaults(keychain: Arc<dyn Vault>, creds: Arc<SystemdCredsFactory>) {
    if let Ok(mut guard) = keychain_slot().write() {
        *guard = keychain;
    }
    if let Ok(mut guard) = creds_slot().write() {
        *guard = creds;
    }
}

/// The vault holding items for `home`, or `None` for the file.
fn vault_for(home: Home, dir: &Path) -> Result<Option<(Arc<dyn Vault>, KeychainItemId)>> {
    let poisoned = || LibraryError::Other(anyhow::anyhow!("credential store lock poisoned"));
    match home {
        Home::File => Ok(None),
        Home::Keychain(item) => keychain_slot()
            .read()
            .map(|guard| Some((Arc::clone(&guard), item)))
            .map_err(|_| poisoned()),
        Home::SystemdCreds(item) => creds_slot()
            .read()
            .map(|factory| Some((factory(dir), item)))
            .map_err(|_| poisoned()),
    }
}

/// Path to `{files_dir}/database.credential`.
#[must_use]
pub fn database_credential_path(files_dir: &Path) -> PathBuf {
    files_dir.join(DATABASE_CREDENTIAL_FILE_NAME)
}

/// SHA-256 of the DEK.
fn fingerprint(dek: &MasterKey) -> [u8; FINGERPRINT_LEN] {
    use sha2::{Digest, Sha256};
    // Equality fingerprint of the 32-byte DEK, not a password KDF.
    // codeql[rust/weak-sensitive-data-hashing]
    Sha256::digest(dek.as_bytes()).into()
}

/// Seals `credential` with `dek`: nonce then ciphertext.
fn seal(credential: &DatabaseCredential, dek: &MasterKey) -> Result<Zeroizing<Vec<u8>>> {
    let payload = Payload {
        v: PAYLOAD_VERSION,
        token: credential.token().to_string(),
        token_id: credential.token_id.clone(),
    };
    let json = Zeroizing::new(
        serde_json::to_vec(&payload).map_err(|err| LibraryError::Other(err.into()))?,
    );
    let mut payload = payload;
    zeroize::Zeroize::zeroize(&mut payload.token);
    let (ciphertext, nonce) = seal_with_dek(&json, dek)?;
    let mut blob = Zeroizing::new(nonce);
    blob.extend_from_slice(&ciphertext);
    Ok(blob)
}

/// Opens a sealed blob (nonce then ciphertext) with `dek`.
fn unseal(blob: &[u8], dek: &MasterKey, source: &str) -> Result<DatabaseCredential> {
    if blob.len() <= NONCE_LEN {
        return Err(LibraryError::Other(anyhow::anyhow!(
            "{source} is truncated"
        )));
    }
    let (nonce, ciphertext) = blob.split_at(NONCE_LEN);
    let json = Zeroizing::new(unseal_with_dek(ciphertext, nonce, dek)?);
    let mut payload: Payload = serde_json::from_slice(&json).map_err(|err| {
        LibraryError::Other(anyhow::anyhow!(
            "{source} holds an unreadable credential: {err}"
        ))
    })?;
    let credential = if payload.v == PAYLOAD_VERSION {
        DatabaseCredential::new(&payload.token, payload.token_id.as_deref())
    } else {
        Err(LibraryError::Other(anyhow::anyhow!(
            "{source} is credential version {}, which this version can't read",
            payload.v
        )))
    };
    zeroize::Zeroize::zeroize(&mut payload.token);
    credential
}

/// The parsed `database.credential`.
struct Record {
    /// The DEK fingerprint it was sealed under.
    fingerprint: [u8; FINGERPRINT_LEN],
    /// Where the sealed blob lives.
    home: Home,
    /// The sealed blob, for a file record.
    blob: Zeroizing<Vec<u8>>,
}

/// Parses `database.credential`'s bytes.
fn parse_record(raw: &[u8], path: &Path) -> Result<Record> {
    let damaged = |what: &str| {
        LibraryError::Other(anyhow::anyhow!(
            "database credential file {} is {what}",
            path.display()
        ))
    };
    let (head, rest) = raw
        .split_at_checked(4)
        .ok_or_else(|| damaged("truncated"))?;
    let (print, rest) = rest
        .split_at_checked(FINGERPRINT_LEN)
        .ok_or_else(|| damaged("truncated"))?;
    let mut fingerprint = [0u8; FINGERPRINT_LEN];
    fingerprint.copy_from_slice(print);
    let item = || -> Result<KeychainItemId> {
        let bytes: [u8; ITEM_LEN] = rest.try_into().map_err(|_| damaged("a damaged pointer"))?;
        Ok(KeychainItemId::from_bytes(bytes))
    };
    let home = match head {
        h if h == MAGIC_FILE => Home::File,
        h if h == MAGIC_KEYCHAIN => Home::Keychain(item()?),
        h if h == MAGIC_CREDS => Home::SystemdCreds(item()?),
        _ => return Err(damaged("not a credential file (expected BCD1/BCD3/BCD4)")),
    };
    let blob = Zeroizing::new(match home {
        Home::File => rest.to_vec(),
        Home::Keychain(_) | Home::SystemdCreds(_) => Vec::new(),
    });
    Ok(Record {
        fingerprint,
        home,
        blob,
    })
}

/// Encodes `database.credential`'s bytes for a blob kept in `home`.
fn encode_record(
    fingerprint: &[u8; FINGERPRINT_LEN],
    home: Home,
    blob: &[u8],
) -> Zeroizing<Vec<u8>> {
    let mut out = Zeroizing::new(Vec::with_capacity(4 + FINGERPRINT_LEN + blob.len()));
    let (magic, tail): (&[u8; 4], &[u8]) = match &home {
        Home::File => (MAGIC_FILE, blob),
        Home::Keychain(item) => (MAGIC_KEYCHAIN, item.as_bytes()),
        Home::SystemdCreds(item) => (MAGIC_CREDS, item.as_bytes()),
    };
    out.extend_from_slice(magic);
    out.extend_from_slice(fingerprint);
    out.extend_from_slice(tail);
    out
}

/// Reads the parsed record, or `None` when there is no credential file.
fn read_record(path: &Path) -> Result<Option<Record>> {
    match std::fs::read(path) {
        Ok(raw) => {
            let raw = Zeroizing::new(raw);
            parse_record(&raw, path).map(Some)
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(LibraryError::Other(anyhow::anyhow!(
            "failed to read database credential {}: {err}",
            path.display()
        ))),
    }
}

/// Deletes the item `home` names, logging (not returning) a failure.
fn delete_item_logged(home: Home, dir: &Path, what: &str) {
    let deleted = vault_for(home, dir).and_then(|vault| match vault {
        Some((vault, item)) => vault.delete(item).map_err(store_error),
        None => Ok(()),
    });
    if let Err(err) = deleted {
        tracing::warn!("could not delete {what} ({}): {err}", home.describe());
    }
}

/// Converts a keystore failure (which never carries item bytes).
fn store_error(err: bookclerk_keystore::KeystoreError) -> LibraryError {
    LibraryError::Other(anyhow::anyhow!("{err}"))
}

/// The store the master key in `files_dir` lives in.
fn master_key_store(files_dir: &Path) -> Result<KeyStoreKind> {
    inspect_master_key(files_dir)?
        .map(|format| format.store())
        .ok_or_else(|| {
            LibraryError::Other(anyhow::anyhow!(
                "there is no master key in {}; start the library once first",
                files_dir.display()
            ))
        })
}

/// [`store_database_credential`] for a DEK given as raw bytes: what a
/// joining shared branch received from its library with the token.
///
/// # Arguments
///
/// * `files_dir` - the files directory holding `master.key`.
/// * `dek` - the library's 32-byte data-encryption key.
/// * `credential` - the token to keep.
///
/// # Errors
///
/// [`LibraryError::Invalid`] when `dek` is not 32 bytes, and whatever
/// [`store_database_credential`] returns.
pub fn store_database_credential_for_dek(
    files_dir: &Path,
    dek: &[u8],
    credential: &DatabaseCredential,
) -> Result<KeyStoreKind> {
    let bytes: [u8; 32] = dek
        .try_into()
        .map_err(|_| LibraryError::Invalid("a data-encryption key is 32 bytes".into()))?;
    let key = MasterKey::from_bytes(bytes);
    store_database_credential(files_dir, &key, credential)
}

/// Seals `credential` with `dek` and keeps it in the master key's store,
/// replacing (and deleting the item of) any credential stored before.
///
/// The new item is written and read back before `database.credential`
/// names it; a failure leaves the previous credential as it was.
///
/// # Arguments
///
/// * `files_dir` - the files directory holding `master.key`.
/// * `dek` - the library's data-encryption key (from `master.key`).
/// * `credential` - the token to keep.
///
/// # Returns
///
/// The store the sealed credential went to.
///
/// # Errors
///
/// Returns an error when there is no master key, the store can't take the
/// item, or the file can't be written.
pub fn store_database_credential(
    files_dir: &Path,
    dek: &MasterKey,
    credential: &DatabaseCredential,
) -> Result<KeyStoreKind> {
    let path = database_credential_path(files_dir);
    let target = master_key_store(files_dir)?;
    let previous = read_record(&path).ok().flatten().map(|record| record.home);
    let blob = seal(credential, dek)?;
    let home = match target {
        KeyStoreKind::File => Home::File,
        KeyStoreKind::Keychain => Home::Keychain(KeychainItemId::random()),
        KeyStoreKind::SystemdCreds => Home::SystemdCreds(KeychainItemId::random()),
    };
    if let Some((vault, item)) = vault_for(home, files_dir)? {
        vault.set(item, &blob).map_err(store_error)?;
        match vault.get(item) {
            Ok(back) if back.as_slice() == blob.as_slice() => {}
            Ok(_) => {
                delete_item_logged(home, files_dir, "a credential item that read back wrong");
                return Err(LibraryError::Other(anyhow::anyhow!(
                    "{} returned different bytes than were written",
                    home.describe()
                )));
            }
            Err(err) => {
                delete_item_logged(home, files_dir, "a credential item that could not be read");
                return Err(store_error(err));
            }
        }
    }
    let record = encode_record(&fingerprint(dek), home, &blob);
    if let Err(err) = crate::master_key::write_secret_file_atomic(&path, &record) {
        delete_item_logged(
            home,
            files_dir,
            "a credential item whose file was not written",
        );
        return Err(err);
    }
    if let Some(old) = previous.filter(|old| *old != home) {
        delete_item_logged(old, files_dir, "the replaced database credential");
    }
    tracing::info!(
        path = %path.display(),
        store = home.kind().as_str(),
        "stored the database credential in {}",
        home.describe()
    );
    Ok(home.kind())
}

/// Reads and unseals the stored credential.
///
/// # Arguments
///
/// * `files_dir` - the files directory holding `database.credential`.
/// * `dek` - the library's data-encryption key.
///
/// # Returns
///
/// The credential, or `None` when none is stored.
///
/// # Errors
///
/// Fails closed: a damaged file, a missing or unreadable item, or a
/// credential sealed under another key is an error.
pub fn load_database_credential(
    files_dir: &Path,
    dek: &MasterKey,
) -> Result<Option<DatabaseCredential>> {
    let path = database_credential_path(files_dir);
    let Some(record) = read_record(&path)? else {
        return Ok(None);
    };
    if record.fingerprint != fingerprint(dek) {
        return Err(LibraryError::Other(anyhow::anyhow!(
            "database credential {} was sealed under a different master key; run \
             `bookclerk config database setup d1` again",
            path.display()
        )));
    }
    let source = format!("database credential {}", path.display());
    let blob = match vault_for(record.home, files_dir)? {
        None => record.blob,
        Some((vault, item)) => vault.get(item).map_err(|err| {
            LibraryError::Other(anyhow::anyhow!(
                "{source} keeps the token in {}, which could not be read: {err}",
                record.home.describe()
            ))
        })?,
    };
    unseal(&blob, dek, &source).map(Some)
}

/// Removes the stored credential and its item.
///
/// # Arguments
///
/// * `files_dir` - the files directory holding `database.credential`.
///
/// # Returns
///
/// True when a credential was removed.
///
/// # Errors
///
/// Returns an error when the file exists but can't be read or removed.
pub fn remove_database_credential(files_dir: &Path) -> Result<bool> {
    let path = database_credential_path(files_dir);
    let Some(record) = read_record(&path)? else {
        return Ok(false);
    };
    std::fs::remove_file(&path).map_err(|err| {
        LibraryError::Other(anyhow::anyhow!(
            "failed to remove database credential {}: {err}",
            path.display()
        ))
    })?;
    delete_item_logged(record.home, files_dir, "the removed database credential");
    Ok(true)
}

/// Moves a stored credential to the master key's current store, after the
/// master key itself moved. Does nothing when none is stored or it is
/// already there.
///
/// # Arguments
///
/// * `files_dir` - the files directory holding both files.
/// * `dek` - the library's data-encryption key.
///
/// # Errors
///
/// Returns an error when the credential can't be read or stored; it then
/// stays where it was.
pub fn rehome_database_credential(files_dir: &Path, dek: &MasterKey) -> Result<()> {
    let path = database_credential_path(files_dir);
    let Some(record) = read_record(&path)? else {
        return Ok(());
    };
    if record.home.kind() == master_key_store(files_dir)? {
        return Ok(());
    }
    let Some(credential) = load_database_credential(files_dir, dek)? else {
        return Ok(());
    };
    store_database_credential(files_dir, dek, &credential).map(|_| ())
}

/// The Cloudflare D1 API token: `BOOKCLERK_D1_API_TOKEN` or
/// `CLOUDFLARE_API_TOKEN` when set, else the credential
/// `bookclerk config database setup d1` stored. A stored token is registered for
/// log redaction.
///
/// The master key is loaded only when a stored credential exists, so a
/// host without one never mints a key here.
///
/// # Arguments
///
/// * `files_dir` - the files directory holding `master.key` and
///   `database.credential`.
///
/// # Errors
///
/// Returns an error when no token is configured, or the stored one can't
/// be read.
pub fn resolve_d1_api_token(files_dir: &Path) -> Result<Zeroizing<String>> {
    let env = bookclerk_config::resolve_d1_api_token().ok();
    if env.is_none() && database_credential_path(files_dir).is_file() {
        let dek = crate::master_key::require_master_key(Some(files_dir))?;
        return resolve_d1_token_with(None, files_dir, Some(&dek));
    }
    resolve_d1_token_with(env, files_dir, None)
}

/// The D1 credential this install uses, for a shared branch joining it:
/// the environment's token (with no id) when one is set, else the stored
/// credential, else `None`.
///
/// # Arguments
///
/// * `files_dir` - the files directory holding `database.credential`.
/// * `dek` - the library's data-encryption key.
///
/// # Errors
///
/// [`LibraryError`] when the environment's token is malformed or the stored
/// credential can't be read.
pub fn current_d1_credential(
    files_dir: &Path,
    dek: &MasterKey,
) -> Result<Option<DatabaseCredential>> {
    current_d1_credential_with(
        bookclerk_config::resolve_d1_api_token().ok(),
        files_dir,
        dek,
    )
}

/// [`current_d1_credential`] with the environment's token passed in.
fn current_d1_credential_with(
    env_token: Option<String>,
    files_dir: &Path,
    dek: &MasterKey,
) -> Result<Option<DatabaseCredential>> {
    match env_token {
        Some(token) => DatabaseCredential::new(&token, None).map(Some),
        None => load_database_credential(files_dir, dek),
    }
}

/// [`resolve_d1_api_token`] with the environment's token passed in.
fn resolve_d1_token_with(
    env_token: Option<String>,
    files_dir: &Path,
    dek: Option<&MasterKey>,
) -> Result<Zeroizing<String>> {
    if let Some(token) = env_token {
        return Ok(Zeroizing::new(token));
    }
    let stored = match dek {
        Some(dek) => load_database_credential(files_dir, dek)?,
        None => None,
    };
    match stored {
        Some(credential) => {
            bookclerk_config::register_secret(credential.token());
            Ok(Zeroizing::new(credential.token().to_string()))
        }
        None => Err(LibraryError::Invalid(
            "D1 API token not configured: run `bookclerk config database setup d1`, or set \
             BOOKCLERK_D1_API_TOKEN or CLOUDFLARE_API_TOKEN (see docs/server/database.md)"
                .into(),
        )),
    }
}

/// True when a D1 token is available: from the environment or stored.
///
/// # Arguments
///
/// * `files_dir` - the files directory holding `database.credential`.
#[must_use]
pub fn d1_api_token_configured(files_dir: &Path) -> bool {
    bookclerk_config::resolve_d1_api_token().is_ok()
        || database_credential_path(files_dir).is_file()
}

#[cfg(test)]
mod tests;
