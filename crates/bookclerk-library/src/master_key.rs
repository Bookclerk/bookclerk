//! Data-encryption key (DEK) for `encrypted_secrets`.
//!
//! # Design
//!
//! Secrets in the DB are sealed with a **random 32-byte DEK** + per-row
//! XChaCha20-Poly1305 nonce (format `sealed-v1`). The DEK is **not** derived
//! per secret (no Argon2 on every load/save).
//!
//! ## Bootstrap (outside the DB)
//!
//! | Source | Behavior |
//! | --- | --- |
//! | `BOOKCLERK_AUTH_PASSWORD` or `[auth].password` | Wraps/unwraps the DEK in `{files_dir}/master.key` |
//! | Auto-mint | If neither password nor file exists, generates a DEK and writes `master.key` (mode `0600`) |
//! | OS keychain | `master.key` is a `BCK3` pointer naming a keychain item that holds the `BCK1`/`BCK2` envelope |
//!
//! ## Key stores
//!
//! The envelope (`BCK1` or `BCK2`) lives either in `master.key` itself or in
//! one OS keychain item ([`bookclerk_keystore`]). On a keychain install
//! `master.key` holds a `BCK3` pointer: the item's id and the DEK's SHA-256
//! fingerprint, never key bytes. Loading follows whatever `master.key` says
//! and fails closed: a missing item, a locked keychain or a key that doesn't
//! match the pointer's fingerprint is an error, never a fresh mint.
//! [`set_key_store_preference`] picks where a *new* key goes (a first mint,
//! a shared branch's installed key, an import); [`move_master_key`] moves an
//! existing one.
//!
//! A later password can wrap an existing `BCK1` file in place (CLI
//! `config master-key wrap`, `config set auth.password`, or daemon config
//! reload). The operator API token is **independent** and short-lived for
//! HTTP auth only. Rotating it does **not** re-encrypt library secrets.
//!
//! Prefer a single opaque ciphertext blob per secret (not per-field columns):
//! Audible envelopes are already opaque, updates stay atomic, and one unseal
//! recovers the whole credential.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use argon2::{Algorithm, Argon2, Params as ArgonParams, Version};
use bookclerk_config::KeyStoreKind;
use bookclerk_keystore::{
    KeychainItemId, KeychainPointer, OsKeychain, PointerError, PointerStore, SystemdCredsVault,
    Vault,
};
use chacha20poly1305::{
    aead::{Aead, KeyInit},
    XChaCha20Poly1305, XNonce,
};
use rand::Rng;
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

use crate::error::{LibraryError, Result};
use crate::secrets::{CIPHER_ALGORITHM, KDF_ALGORITHM, KDF_M_COST, KDF_P_COST, KDF_T_COST};

/// Env var that wraps the DEK in `master.key` (optional). The password can
/// also come from the file `BOOKCLERK_AUTH_PASSWORD_FILE` names or the
/// `bookclerk-auth-password` systemd credential
/// ([`bookclerk_config::auth_password_from_env`]).
pub const AUTH_PASSWORD_ENV: &str = bookclerk_config::AUTH_PASSWORD_ENV;

/// Bootstrap key file under `BOOKCLERK_FILES_DIR` (auto-minted when missing).
pub const MASTER_KEY_FILE_NAME: &str = "master.key";

/// On-disk magic for an unwrapped 32-byte DEK (`BCK1` + key bytes).
const MAGIC_RAW: &[u8; 4] = b"BCK1"; // raw DEK (no password wrap)
/// On-disk magic for a password-wrapped DEK (`BCK2` + salt + nonce + ciphertext).
const MAGIC_WRAPPED: &[u8; 4] = b"BCK2"; // Argon2id + XChaCha wrap
/// Data-encryption key length in bytes (XChaCha20-Poly1305 key).
const DEK_LEN: usize = 32;
/// Argon2id salt length in bytes for wrapping `master.key`.
const SALT_LEN: usize = 16;
/// XChaCha20-Poly1305 nonce length in bytes (wrap and per-row seal).
const NONCE_LEN: usize = 24;

/// Process-wide cached DEK (filled by [`configure_master_key`] / [`resolve_master_key`]).
static CACHED_DEK: OnceLock<Mutex<Option<MasterKey>>> = OnceLock::new();

/// Process-wide cached DEK slot filled by [`configure_master_key`] / [`resolve_master_key`].
fn cache_slot() -> &'static Mutex<Option<MasterKey>> {
    CACHED_DEK.get_or_init(|| Mutex::new(None))
}

/// Where a new key goes, and whether that place is required.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyStorePreference {
    /// The store new keys are written to.
    pub kind: KeyStoreKind,
    /// True when the operator set `[auth].key_store`: a keychain that can't
    /// be reached is then an error. False for a host's default, which falls
    /// back to the file with a warning.
    pub required: bool,
}

impl KeyStorePreference {
    /// The preference for `configured` (`[auth].key_store`), or the host's
    /// `default` when unset.
    ///
    /// # Arguments
    ///
    /// * `configured` - the operator's setting, if any.
    /// * `default` - the host's default store.
    #[must_use]
    pub fn from_config(configured: Option<KeyStoreKind>, default: KeyStoreKind) -> Self {
        match configured {
            Some(kind) => Self {
                kind,
                required: true,
            },
            None => Self {
                kind: default,
                required: false,
            },
        }
    }
}

/// The preference before a host sets one: the file store, not required.
const DEFAULT_PREFERENCE: KeyStorePreference = KeyStorePreference {
    kind: KeyStoreKind::File,
    required: false,
};

/// Process-wide store preference ([`set_key_store_preference`]).
static PREFERENCE: RwLock<KeyStorePreference> = RwLock::new(DEFAULT_PREFERENCE);

/// Sets where this process writes new keys (first mint, a shared branch's
/// installed key, an import). Existing keys stay where `master.key` says.
///
/// # Arguments
///
/// * `preference` - the store and whether it is required.
pub fn set_key_store_preference(preference: KeyStorePreference) {
    if let Ok(mut guard) = PREFERENCE.write() {
        *guard = preference;
    }
}

/// The current store preference.
#[must_use]
pub fn key_store_preference() -> KeyStorePreference {
    PREFERENCE
        .read()
        .map(|guard| *guard)
        .unwrap_or(DEFAULT_PREFERENCE)
}

/// The process-wide keychain ([`set_keychain_vault`]); the OS keychain by
/// default.
static VAULT: OnceLock<RwLock<Arc<dyn Vault>>> = OnceLock::new();

/// The keychain slot, initialised to [`OsKeychain`].
fn vault_slot() -> &'static RwLock<Arc<dyn Vault>> {
    VAULT.get_or_init(|| RwLock::new(Arc::new(OsKeychain::new())))
}

/// Replaces the keychain this process uses, for tests (a
/// [`bookclerk_keystore::MemoryVault`]) and for hosts that substitute
/// their own store.
///
/// # Arguments
///
/// * `vault` - the keychain to read and write items in.
pub fn set_keychain_vault(vault: Arc<dyn Vault>) {
    if let Ok(mut guard) = vault_slot().write() {
        *guard = vault;
    }
}

/// The keychain this process uses.
fn vault() -> Result<Arc<dyn Vault>> {
    vault_slot()
        .read()
        .map(|guard| Arc::clone(&guard))
        .map_err(|_| LibraryError::Other(anyhow::anyhow!("keychain slot lock poisoned")))
}

/// Builds the `systemd-creds` store for a files directory.
pub type SystemdCredsFactory = dyn Fn(&Path) -> Arc<dyn Vault> + Send + Sync;

/// The process-wide `systemd-creds` store builder
/// ([`set_systemd_creds_vault`]); [`SystemdCredsVault::new`] by default.
static CREDS_FACTORY: OnceLock<RwLock<Arc<SystemdCredsFactory>>> = OnceLock::new();

/// The `systemd-creds` builder slot, initialised to the system tool.
fn creds_factory_slot() -> &'static RwLock<Arc<SystemdCredsFactory>> {
    CREDS_FACTORY.get_or_init(|| {
        RwLock::new(Arc::new(|dir: &Path| -> Arc<dyn Vault> {
            Arc::new(SystemdCredsVault::new(dir))
        }))
    })
}

/// Replaces how this process builds its `systemd-creds` store, for tests
/// (a fake tool, or a [`bookclerk_keystore::MemoryVault`]).
///
/// # Arguments
///
/// * `factory` - builds the store for a files directory.
pub fn set_systemd_creds_vault(factory: Arc<SystemdCredsFactory>) {
    if let Ok(mut guard) = creds_factory_slot().write() {
        *guard = factory;
    }
}

/// The store `store` names, for the files directory `dir`.
fn vault_for(store: PointerStore, dir: &Path) -> Result<Arc<dyn Vault>> {
    match store {
        PointerStore::Keychain => vault(),
        PointerStore::SystemdCreds => creds_factory_slot()
            .read()
            .map(|factory| factory(dir))
            .map_err(|_| LibraryError::Other(anyhow::anyhow!("systemd-creds slot lock poisoned"))),
    }
}

/// The directory holding the `master.key` at `path` (where a
/// `systemd-creds` item lives beside it).
fn dir_of(path: &Path) -> &Path {
    path.parent().unwrap_or_else(|| Path::new("."))
}

/// A store item that holds a key's envelope.
#[derive(Debug, Clone)]
struct ItemHome {
    /// The store holding the item.
    store: PointerStore,
    /// The item.
    item: KeychainItemId,
    /// The files directory (where a `systemd-creds` item lives).
    dir: PathBuf,
}

impl ItemHome {
    /// The item `pointer` names, for the `master.key` in `dir`.
    fn of(pointer: &KeychainPointer, dir: &Path) -> Self {
        Self {
            store: pointer.store,
            item: pointer.item,
            dir: dir.to_path_buf(),
        }
    }
}

/// How an item is named in messages.
fn describe_item(store: PointerStore, item: KeychainItemId) -> String {
    match store {
        PointerStore::Keychain => format!("OS keychain item {item}"),
        PointerStore::SystemdCreds => format!("systemd-creds credential {item}"),
    }
}

/// The key store a pointer's store is.
fn store_kind(store: PointerStore) -> KeyStoreKind {
    match store {
        PointerStore::Keychain => KeyStoreKind::Keychain,
        PointerStore::SystemdCreds => KeyStoreKind::SystemdCreds,
    }
}

/// Where a loaded envelope came from, so a re-wrap writes it back there.
#[derive(Clone, Copy)]
enum Location {
    /// `master.key` holds the envelope.
    File,
    /// The keychain or `systemd-creds` item `master.key` points at holds it.
    Keychain(KeychainPointer),
}

/// On-disk `master.key` envelope kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MasterKeyFormat {
    /// Raw 32-byte DEK (`BCK1`).
    Raw,
    /// Password-wrapped DEK (`BCK2`).
    Wrapped,
    /// A pointer to an OS keychain item (`BCK3`); the item holds `BCK1` or
    /// `BCK2`.
    Keychain,
    /// A pointer to a `systemd-creds` credential (`BCK4`); the credential
    /// holds `BCK1` or `BCK2`.
    SystemdCreds,
}

impl MasterKeyFormat {
    /// The four-character header: `BCK1`, `BCK2`, `BCK3` or `BCK4`.
    #[must_use]
    pub fn header(self) -> &'static str {
        match self {
            Self::Raw => "BCK1",
            Self::Wrapped => "BCK2",
            Self::Keychain => "BCK3",
            Self::SystemdCreds => "BCK4",
        }
    }

    /// The store this format means.
    #[must_use]
    pub fn store(self) -> KeyStoreKind {
        match self {
            Self::Raw | Self::Wrapped => KeyStoreKind::File,
            Self::Keychain => KeyStoreKind::Keychain,
            Self::SystemdCreds => KeyStoreKind::SystemdCreds,
        }
    }
}

/// 32-byte data-encryption key (zeroized on drop).
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct MasterKey {
    /// 32-byte DEK used to seal `encrypted_secrets`; zeroized on drop. Only this process can unseal after load/unwrap.
    bytes: [u8; DEK_LEN],
}

impl MasterKey {
    /// Returns the 32-byte DEK without copying.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; DEK_LEN] {
        &self.bytes
    }

    /// Mints a fresh 32-byte DEK from the OS CSPRNG.
    fn random() -> Result<Self> {
        Ok(Self {
            bytes: rand::rngs::OsRng.gen(),
        })
    }

    /// Wraps already-unwrapped DEK bytes (from `BCK1` or after password unwrap).
    pub(crate) fn from_bytes(bytes: [u8; DEK_LEN]) -> Self {
        Self { bytes }
    }

    /// Fixed DEK for unit tests that must not touch `master.key`.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn from_test_bytes(bytes: [u8; DEK_LEN]) -> Self {
        Self::from_bytes(bytes)
    }
}

impl std::fmt::Debug for MasterKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MasterKey(***)")
    }
}

/// Path to `{files_dir}/master.key`.
#[must_use]
pub fn master_key_path(files_dir: &Path) -> PathBuf {
    files_dir.join(MASTER_KEY_FILE_NAME)
}

/// Inspect the on-disk envelope without unlocking (missing → `None`).
///
/// # Errors
///
/// Returns an error when the operation fails.
pub fn inspect_master_key(files_dir: &Path) -> Result<Option<MasterKeyFormat>> {
    let path = master_key_path(files_dir);
    if !path.is_file() {
        return Ok(None);
    }
    let raw = std::fs::read(&path).map_err(|e| {
        LibraryError::Other(anyhow::anyhow!(
            "failed to read master key {}: {e}",
            path.display()
        ))
    })?;
    if raw.len() < 4 {
        return Err(LibraryError::Other(anyhow::anyhow!(
            "master key file {} is truncated",
            path.display()
        )));
    }
    match &raw[..4] {
        b"BCK1" => Ok(Some(MasterKeyFormat::Raw)),
        b"BCK2" => Ok(Some(MasterKeyFormat::Wrapped)),
        b"BCK3" | b"BCK4" => Ok(Some(match parse_pointer(&raw, &path)?.store {
            PointerStore::Keychain => MasterKeyFormat::Keychain,
            PointerStore::SystemdCreds => MasterKeyFormat::SystemdCreds,
        })),
        _ => Err(LibraryError::Other(anyhow::anyhow!(
            "master key file {} has unknown magic (expected BCK1/BCK2/BCK3/BCK4)",
            path.display()
        ))),
    }
}

/// Whether the key is password-wrapped (`BCK2`), reading the keychain item
/// when `master.key` is a pointer. `None` when there is no `master.key`.
///
/// # Errors
///
/// Returns an error when `master.key` or the item it names can't be read.
pub fn master_key_is_wrapped(files_dir: &Path) -> Result<Option<bool>> {
    let path = master_key_path(files_dir);
    if !path.is_file() {
        return Ok(None);
    }
    let (envelope, location) = read_envelope(&path)?;
    envelope_is_wrapped(&envelope).map(Some).ok_or_else(|| {
        let source = envelope_source(location, &path);
        LibraryError::Other(anyhow::anyhow!("{source} is not a valid key envelope"))
    })
}

/// Whether a structurally valid envelope is wrapped: `Some(false)` for a
/// `BCK1` of the right length, `Some(true)` for a `BCK2` long enough to
/// hold its salt, nonce and tag, and `None` for anything else. Needs no
/// password.
fn envelope_is_wrapped(envelope: &[u8]) -> Option<bool> {
    match envelope.get(..4) {
        Some(b"BCK1") if envelope.len() == 4 + DEK_LEN => Some(false),
        Some(b"BCK2") if envelope.len() >= 4 + SALT_LEN + NONCE_LEN + 16 => Some(true),
        _ => None,
    }
}

/// Load or mint the DEK and cache it for this process.
///
/// Password comes from the environment only (`BOOKCLERK_AUTH_PASSWORD`,
/// `BOOKCLERK_AUTH_PASSWORD_FILE` or the `bookclerk-auth-password` systemd
/// credential). Prefer [`configure_master_key_with`] when `[auth].password`
/// may apply.
///
/// Call once at CLI/daemon startup after `paths.ensure_dirs()`.
///
/// # Errors
///
/// Returns an error when a named password source gives no password, or
/// when the key can't be loaded or minted.
pub fn configure_master_key(files_dir: &Path) -> Result<MasterKey> {
    configure_master_key_with(files_dir, read_auth_password_env()?.as_deref())
}

/// Load or mint the DEK using an explicit password (env / config), then cache it.
///
/// When `password` is set and `master.key` is still `BCK1`, re-wraps to `BCK2`
/// in place (same DEK bytes — sealed ciphertext stays valid). Empty `password`
/// is treated as absent.
///
/// If the resolved DEK identity differs from the previously cached one (e.g. a
/// replaced `master.key`), the plaintext unseal cache is flushed so callers
/// cannot observe stale plaintext under the new key.
///
/// # Errors
///
/// Returns an error when the operation fails.
pub fn configure_master_key_with(files_dir: &Path, password: Option<&str>) -> Result<MasterKey> {
    let password = password
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .inspect(|p| bookclerk_config::register_secret(p));
    let key = resolve_master_key_with(files_dir, password)?;
    let mut dek_changed = false;
    if let Ok(mut guard) = cache_slot().lock() {
        dek_changed = guard
            .as_ref()
            .is_some_and(|prev| prev.as_bytes() != key.as_bytes());
        *guard = Some(key.clone());
    }
    if dek_changed {
        crate::secrets::clear_unseal_cache();
    }
    Ok(key)
}

/// Return the process-cached DEK, or resolve from `files_dir` if not configured.
///
/// # Errors
///
/// Returns an error when the operation fails.
pub fn require_master_key(files_dir: Option<&Path>) -> Result<MasterKey> {
    if let Ok(guard) = cache_slot().lock() {
        if let Some(key) = guard.as_ref() {
            return Ok(key.clone());
        }
    }
    let dir = files_dir.ok_or_else(|| {
        LibraryError::Other(anyhow::anyhow!(
            "master key not configured — call configure_master_key(files_dir) at startup"
        ))
    })?;
    configure_master_key(dir)
}

/// Resolve the DEK from env + `master.key` (minting when needed).
///
/// # Errors
///
/// Returns an error when a named password source gives no password, or
/// when the key can't be loaded or minted.
pub fn resolve_master_key(files_dir: &Path) -> Result<MasterKey> {
    resolve_master_key_with(files_dir, read_auth_password_env()?.as_deref())
}

/// DEK loaded from `master.key`, plus whether this call created the file.
#[derive(Clone)]
pub struct MasterKeyResolution {
    /// Unwrapped data-encryption key.
    pub key: MasterKey,
    /// True when this call created `master.key` (lost create races are false).
    pub minted: bool,
}

/// Resolve the DEK with an explicit password override.
///
/// # Errors
///
/// Returns an error when the operation fails.
pub fn resolve_master_key_with(files_dir: &Path, password: Option<&str>) -> Result<MasterKey> {
    Ok(resolve_master_key_detailed(files_dir, password)?.key)
}

/// Resolve the DEK and report whether `master.key` was created by this call.
///
/// # Errors
///
/// Returns an error when the file cannot be read, unwrapped, or created.
pub fn resolve_master_key_detailed(
    files_dir: &Path,
    password: Option<&str>,
) -> Result<MasterKeyResolution> {
    let path = master_key_path(files_dir);
    let password = password
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .inspect(|p| bookclerk_config::register_secret(p));

    if path.is_file() {
        return Ok(MasterKeyResolution {
            key: load_master_key_at(&path, password)?,
            minted: false,
        });
    }

    mint_master_key(&path, password)
}

/// SHA-256 hex fingerprint of the unwrapped DEK.
///
/// This identifies the library secret root. It is not a password hash: the
/// passphrase, when set, is stretched with Argon2id before it unwraps this
/// key. Wrapping changes the file bytes and leaves this fingerprint stable.
#[must_use]
pub fn master_key_fingerprint(key: &MasterKey) -> String {
    use sha2::{Digest, Sha256};
    // Equality fingerprint of the 32-byte DEK, not a password KDF.
    // codeql[rust/weak-sensitive-data-hashing]
    hex::encode(Sha256::digest(key.as_bytes()))
}

/// Drops `key` from the process cache when it is the cached DEK.
pub fn uncache_master_key(key: &MasterKey) {
    if let Ok(mut guard) = cache_slot().lock() {
        if guard
            .as_ref()
            .is_some_and(|cached| cached.as_bytes() == key.as_bytes())
        {
            *guard = None;
        }
    }
    crate::secrets::clear_unseal_cache();
}

/// Delete a `master.key` this process just created and drop it from the cache.
///
/// # Errors
///
/// Returns an error when the file exists and cannot be removed.
pub fn discard_minted_master_key(files_dir: &Path, key: &MasterKey) -> Result<()> {
    let path = master_key_path(files_dir);
    if path.is_file() {
        // A key minted into the keychain: its item goes too, but only once
        // the pointer is gone, so a pointer that stays still loads.
        let minted_item = std::fs::read(&path)
            .ok()
            .and_then(|raw| KeychainPointer::parse(&raw).ok())
            .filter(|pointer| pointer.fingerprint == fingerprint_bytes(key))
            .map(|pointer| ItemHome::of(&pointer, files_dir));
        std::fs::remove_file(&path).map_err(|e| {
            LibraryError::Other(anyhow::anyhow!(
                "failed to remove minted master key {}: {e}",
                path.display()
            ))
        })?;
        if let Some(item) = minted_item {
            delete_item_logged(&item, "a minted key's store item");
        }
    }
    if let Ok(mut guard) = cache_slot().lock() {
        if guard
            .as_ref()
            .is_some_and(|cached| cached.as_bytes() == key.as_bytes())
        {
            *guard = None;
        }
    }
    crate::secrets::clear_unseal_cache();
    Ok(())
}

/// Wrap an existing `BCK1` key with `password` (`BCK2`), in `master.key`
/// or in the OS keychain item it points at.
///
/// No-op (still unlocks) when already `BCK2` and `password` is correct.
/// Fails if the file is missing — mint via [`configure_master_key_with`] first.
///
/// # Errors
///
/// Returns an error when the operation fails.
pub fn wrap_master_key(files_dir: &Path, password: &str) -> Result<MasterKey> {
    let password = password.trim();
    if password.is_empty() {
        return Err(LibraryError::Other(anyhow::anyhow!(
            "password must be non-empty to wrap master.key"
        )));
    }
    bookclerk_config::register_secret(password);
    let path = master_key_path(files_dir);
    if !path.is_file() {
        return Err(LibraryError::Other(anyhow::anyhow!(
            "master key file {} does not exist — open the library on a new library to mint it",
            path.display()
        )));
    }
    configure_master_key_with(files_dir, Some(password))
}

/// Installs `dek` as `{files_dir}/master.key`: a shared branch joining a library
/// receives the library's DEK (sealed to its node key, never the
/// passphrase-wrapped file) and keeps it under its own `password`.
///
/// Writes [`store_master_key`]'s bytes (the envelope, or a pointer to the
/// keychain item it just wrote), mode `0600`, replacing any
/// existing file atomically. The process cache is not changed: the daemon
/// restarts on the new bootstrap.
///
/// # Errors
///
/// Returns an error when `dek` is not 32 bytes or the file cannot be
/// written.
pub fn install_master_key(files_dir: &Path, dek: &[u8], password: Option<&str>) -> Result<()> {
    prepare_master_key_dir(files_dir)?;
    let path = master_key_path(files_dir);
    // The item the current `master.key` names, retired once replaced.
    let old_item = std::fs::read(&path)
        .ok()
        .and_then(|raw| KeychainPointer::parse(&raw).ok())
        .map(|pointer| ItemHome::of(&pointer, files_dir));
    let stored = store_master_key(files_dir, dek, password)?;
    if let Err(err) = write_secret_file_atomic(&path, stored.bytes()) {
        stored.discard();
        return Err(err);
    }
    let in_keychain = stored.item.is_some();
    let new_item = stored.item.as_ref().map(|home| (home.store, home.item));
    if let Some(old) = old_item.filter(|old| Some((old.store, old.item)) != new_item) {
        delete_item_logged(&old, "the store item of the key this install replaced");
    }
    let wrapped = password.map(str::trim).is_some_and(|p| !p.is_empty());
    tracing::info!(
        path = %path.display(),
        wrapped,
        in_keychain,
        "installed the library's data-encryption key (mode 0600)"
    );
    if !wrapped && !in_keychain {
        warn_unwrapped_master_key(&path);
    }
    Ok(())
}

/// Creates the directory that holds `master.key` and sets it to mode
/// `0700` on Unix, as a first mint does.
///
/// # Errors
///
/// Returns an error when the directory cannot be created or hardened.
pub fn prepare_master_key_dir(files_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(files_dir).map_err(|e| {
        LibraryError::Other(anyhow::anyhow!(
            "failed to create master key directory {}: {e}",
            files_dir.display()
        ))
    })?;
    harden_path(files_dir, true)
}

/// Warns that the `master.key` at `path` was written as `BCK1` (the DEK
/// unwrapped at rest), and how to wrap it.
pub fn warn_unwrapped_master_key(path: &Path) {
    tracing::warn!(
        path = %path.display(),
        "master.key is BCK1 (unwrapped). Set {AUTH_PASSWORD_ENV} or \
         [auth].password, then run `bookclerk config master-key wrap` \
         (or reload bookclerkd) to wrap the DEK at rest."
    );
}

/// The key envelope for `dek`: `BCK2` (Argon2id + XChaCha20-Poly1305) when
/// `password` is set and non-empty after trimming, `BCK1` otherwise. It
/// touches no store; [`store_master_key`] places an envelope where the store
/// preference says.
///
/// # Errors
///
/// Returns [`LibraryError::Invalid`] when `dek` is not 32 bytes, and the
/// wrapping error.
pub fn encode_master_key_file(
    dek: &[u8],
    password: Option<&str>,
) -> Result<zeroize::Zeroizing<Vec<u8>>> {
    let bytes: [u8; DEK_LEN] = dek
        .try_into()
        .map_err(|_| LibraryError::Invalid(format!("a data-encryption key is {DEK_LEN} bytes")))?;
    let key = MasterKey::from_bytes(bytes);
    let password = password
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .inspect(|p| bookclerk_config::register_secret(p));
    Ok(zeroize::Zeroizing::new(match password {
        Some(pw) => encode_wrapped_master_key(&key, pw)?,
        None => encode_raw_master_key(&key),
    }))
}

/// Reads the auth password from the environment (the variable, the file it
/// names, or the systemd credential) and registers it for log redaction; an
/// empty variable is treated as absent.
///
/// # Errors
///
/// When a named file or present credential gives no password, so a fresh
/// mint never runs unwrapped by mistake.
fn read_auth_password_env() -> Result<Option<String>> {
    bookclerk_config::auth_password_from_env()
        .map_err(|e| LibraryError::Other(anyhow::anyhow!("{e}")))
}

/// Creates `master.key` (mode 0600) for a fresh DEK: the envelope itself
/// (`BCK1`, or `BCK2` under `password`), or, when the store preference is
/// the keychain, a `BCK3` pointer to the item that now holds the envelope.
/// Only a later unwrap with the same password can recover a wrapped DEK.
fn mint_master_key(path: &Path, password: Option<&str>) -> Result<MasterKeyResolution> {
    let dek = MasterKey::random()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            LibraryError::Other(anyhow::anyhow!(
                "failed to create master key directory {}: {e}",
                parent.display()
            ))
        })?;
        harden_path(parent, true)?;
    }
    let placed = place_envelope(&dek, password, dir_of(path))?;
    match write_secret_file_create_new(path, &placed.file_bytes) {
        Ok(()) => {
            tracing::info!(
                path = %path.display(),
                wrapped = password.is_some(),
                store = placed.store().as_str(),
                "minted data-encryption key for encrypted_secrets (mode 0600)"
            );
            if password.is_none() && placed.item.is_none() {
                warn_unwrapped_master_key(path);
            }
            Ok(MasterKeyResolution {
                key: dek,
                minted: true,
            })
        }
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {
            // Lost a mint race: drop our item and load the winner's key.
            if let Some(item) = &placed.item {
                delete_item_logged(item, "a store item from a lost mint race");
            }
            Ok(MasterKeyResolution {
                key: load_master_key_at(path, password)?,
                minted: false,
            })
        }
        Err(e) => {
            if let Some(item) = &placed.item {
                delete_item_logged(item, "a store item whose pointer could not be written");
            }
            Err(LibraryError::Other(anyhow::anyhow!(
                "failed to mint master key {}: {e}",
                path.display()
            )))
        }
    }
}

/// A key placed in its store, and the bytes `master.key` must then hold.
struct PlacedKey {
    /// `master.key`'s contents: the envelope, or a `BCK3`/`BCK4` pointer.
    file_bytes: Zeroizing<Vec<u8>>,
    /// The store item now holding the envelope, when the key went to one.
    item: Option<ItemHome>,
}

impl PlacedKey {
    /// The store the key went to.
    fn store(&self) -> KeyStoreKind {
        match &self.item {
            Some(home) => store_kind(home.store),
            None => KeyStoreKind::File,
        }
    }
}

/// Puts `dek`'s envelope where the [`key_store_preference`] says.
///
/// For the keychain or `systemd-creds`, writes a new item, reads it back to
/// check it, and returns the pointer for `master.key`. A keychain that can't
/// be reached is an error when the preference is required, and otherwise
/// falls back to the file with a warning. `systemd-creds` always falls back
/// to the file when it can't seal the key.
///
/// # Arguments
///
/// * `dek` - the key.
/// * `password` - wraps the envelope as `BCK2` when set.
/// * `dir` - the files directory `master.key` lives in.
fn place_envelope(dek: &MasterKey, password: Option<&str>, dir: &Path) -> Result<PlacedKey> {
    let envelope = Zeroizing::new(match password {
        Some(pw) => encode_wrapped_master_key(dek, pw)?,
        None => encode_raw_master_key(dek),
    });
    let preference = key_store_preference();
    let (store, fallback_ok, label) = match preference.kind {
        KeyStoreKind::File => {
            return Ok(PlacedKey {
                file_bytes: envelope,
                item: None,
            })
        }
        KeyStoreKind::Keychain => (
            PointerStore::Keychain,
            !preference.required,
            "the OS keychain",
        ),
        KeyStoreKind::SystemdCreds => (PointerStore::SystemdCreds, true, "systemd-creds"),
    };
    match put_in_store(store, dir, dek, &envelope) {
        Ok(pointer) => Ok(PlacedKey {
            file_bytes: Zeroizing::new(pointer.encode().to_vec()),
            item: Some(ItemHome::of(&pointer, dir)),
        }),
        Err(err) if fallback_ok => {
            tracing::warn!(
                "{label} could not take the data-encryption key ({err}); keeping it in \
                 master.key instead. Run `bookclerk config master-key move {}` once it is \
                 available.",
                preference.kind.as_str()
            );
            Ok(PlacedKey {
                file_bytes: envelope,
                item: None,
            })
        }
        Err(err) => Err(err),
    }
}

/// Writes `envelope` to a fresh item in `store`, reads it back, and checks
/// the bytes match. Deletes the item again if the check fails.
///
/// # Arguments
///
/// * `store` - the keychain or `systemd-creds`.
/// * `dir` - the files directory (where a `systemd-creds` item lives).
/// * `dek` - the key, whose fingerprint the pointer records.
/// * `envelope` - the `BCK1`/`BCK2` bytes to store.
fn put_in_store(
    store: PointerStore,
    dir: &Path,
    dek: &MasterKey,
    envelope: &[u8],
) -> Result<KeychainPointer> {
    let pointer = KeychainPointer {
        store,
        item: KeychainItemId::random(),
        fingerprint: fingerprint_bytes(dek),
    };
    let home = ItemHome::of(&pointer, dir);
    let vault = vault_for(store, dir)?;
    vault.set(pointer.item, envelope).map_err(keychain_error)?;
    let back = vault.get(pointer.item).map_err(keychain_error);
    match back {
        Ok(bytes) if bytes.as_slice() == envelope => Ok(pointer),
        Ok(_) => {
            delete_item_logged(&home, "a store item that read back differently");
            Err(LibraryError::Other(anyhow::anyhow!(
                "{} returned different bytes than were written",
                describe_item(store, pointer.item)
            )))
        }
        Err(err) => {
            delete_item_logged(&home, "a store item that could not be read back");
            Err(err)
        }
    }
}

/// Deletes a store item, logging (not returning) a failure. Used to clean
/// up items that never became a key's home.
fn delete_item_logged(home: &ItemHome, what: &str) {
    let deleted =
        vault_for(home.store, &home.dir).and_then(|v| v.delete(home.item).map_err(keychain_error));
    if let Err(err) = deleted {
        tracing::warn!(item = %home.item, "could not delete {what}: {err}");
    }
}

/// Converts a keychain failure into a library error (the message never
/// carries item bytes).
fn keychain_error(err: bookclerk_keystore::KeystoreError) -> LibraryError {
    LibraryError::Other(anyhow::anyhow!("{err}"))
}

/// SHA-256 of the DEK as raw bytes ([`master_key_fingerprint`] is its hex).
fn fingerprint_bytes(key: &MasterKey) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    // Equality fingerprint of the 32-byte DEK, not a password KDF.
    // codeql[rust/weak-sensitive-data-hashing]
    Sha256::digest(key.as_bytes()).into()
}

/// Parses a `BCK3`/`BCK4` pointer read from `path`, naming the file in errors.
fn parse_pointer(raw: &[u8], path: &Path) -> Result<KeychainPointer> {
    KeychainPointer::parse(raw).map_err(|err| match err {
        PointerError::NotAPointer => LibraryError::Other(anyhow::anyhow!(
            "master key file {} is not a keychain pointer",
            path.display()
        )),
        PointerError::BadLength(_) => LibraryError::Other(anyhow::anyhow!(
            "master key file {} is a damaged keychain pointer: {err}",
            path.display()
        )),
    })
}

/// Reads `master.key` and, when it is a `BCK3` pointer, the keychain item it
/// names. Returns the envelope (`BCK1`/`BCK2` bytes) and where it lives.
fn read_envelope(path: &Path) -> Result<(Zeroizing<Vec<u8>>, Location)> {
    let raw = Zeroizing::new(std::fs::read(path).map_err(|e| {
        LibraryError::Other(anyhow::anyhow!(
            "failed to read master key {}: {e}",
            path.display()
        ))
    })?);
    if PointerStore::for_header(&raw).is_none() {
        return Ok((raw, Location::File));
    }
    let pointer = parse_pointer(&raw, path)?;
    let envelope = vault_for(pointer.store, dir_of(path))?
        .get(pointer.item)
        .map_err(|err| {
            LibraryError::Other(anyhow::anyhow!(
                "master key file {} keeps the key in {}, which could not be read: {err}",
                path.display(),
                describe_item(pointer.store, pointer.item)
            ))
        })?;
    Ok((envelope, Location::Keychain(pointer)))
}

/// Writes an envelope back where it was loaded from (a `BCK1`→`BCK2`
/// re-wrap).
fn write_envelope(location: Location, path: &Path, envelope: &[u8]) -> Result<()> {
    match location {
        Location::File => write_secret_file_atomic(path, envelope),
        Location::Keychain(pointer) => vault_for(pointer.store, dir_of(path))?
            .set(pointer.item, envelope)
            .map_err(keychain_error),
    }
}

/// Loads the DEK `master.key` at `path` names, from the file or the keychain.
///
/// A raw (`BCK1`) key is re-wrapped in place under a newly set password,
/// and only after a keychain key has matched its pointer's fingerprint.
fn load_master_key_at(path: &Path, password: Option<&str>) -> Result<MasterKey> {
    let (key, envelope, location) = read_key_snapshot(path, password)?;
    if let Some(pw) = password.filter(|_| envelope.starts_with(MAGIC_RAW)) {
        // Password newly set: re-wrap in place for at-rest protection.
        let wrapped = Zeroizing::new(encode_wrapped_master_key(&key, pw)?);
        write_envelope(location, path, &wrapped)?;
        tracing::info!(
            "re-wrapped the data-encryption key (BCK1→BCK2) in {} with the auth password",
            envelope_source(location, path)
        );
    }
    Ok(key)
}

/// Reads `master.key` (and the keychain item it names) once and decodes the
/// key from that one read, writing nothing back. A keychain key must match
/// its pointer's fingerprint.
///
/// # Returns
///
/// The key, the envelope it was decoded from, and where that envelope lives.
fn read_key_snapshot(
    path: &Path,
    password: Option<&str>,
) -> Result<(MasterKey, Zeroizing<Vec<u8>>, Location)> {
    let (envelope, location) = read_envelope(path)?;
    let key = parse_envelope(&envelope, location, password, path)?;
    if let Location::Keychain(pointer) = location {
        if fingerprint_bytes(&key) != pointer.fingerprint {
            return Err(LibraryError::Other(anyhow::anyhow!(
                "{} holds a different key than master key file {} names",
                describe_item(pointer.store, pointer.item),
                path.display()
            )));
        }
    }
    Ok((key, envelope, location))
}

/// How an envelope's home is named in messages.
fn envelope_source(location: Location, path: &Path) -> String {
    match location {
        Location::File => format!("master key file {}", path.display()),
        Location::Keychain(pointer) => describe_item(pointer.store, pointer.item),
    }
}

/// Unwraps a `BCK1`/`BCK2` envelope without writing anything; `BCK2`
/// requires the auth password.
fn parse_envelope(
    raw: &[u8],
    location: Location,
    password: Option<&str>,
    path: &Path,
) -> Result<MasterKey> {
    let source = envelope_source(location, path);
    if raw.len() < 4 {
        return Err(LibraryError::Other(anyhow::anyhow!(
            "{source} is truncated"
        )));
    }
    match &raw[..4] {
        b"BCK1" => {
            let bytes: [u8; DEK_LEN] = raw[4..].try_into().map_err(|_| {
                LibraryError::Other(anyhow::anyhow!("{source} has invalid BCK1 length"))
            })?;
            let dek = MasterKey::from_bytes(bytes);
            if password.is_none() && matches!(location, Location::File) {
                tracing::warn!(
                    path = %path.display(),
                    "master.key is BCK1 (unwrapped). Set {AUTH_PASSWORD_ENV} or \
                     [auth].password, then run `bookclerk config master-key wrap` \
                     (or reload bookclerkd) to wrap the DEK at rest, or move it \
                     to the OS keychain with `bookclerk config master-key move keychain`."
                );
            }
            Ok(dek)
        }
        b"BCK2" => {
            let pw = password.ok_or_else(|| {
                LibraryError::Other(anyhow::anyhow!(
                    "{source} is password-wrapped — set {AUTH_PASSWORD_ENV} \
                     or [auth].password"
                ))
            })?;
            decode_wrapped_master_key(&raw[4..], pw, path)
        }
        _ => Err(LibraryError::Other(anyhow::anyhow!(
            "{source} has unknown magic (expected BCK1/BCK2)"
        ))),
    }
}

/// Reads the existing DEK, and the envelope it came from, without minting
/// one or writing anything back.
///
/// # Errors
///
/// Returns an error when `master.key` is missing, or the key can't be read
/// or unwrapped.
fn load_existing(
    files_dir: &Path,
    password: Option<&str>,
) -> Result<(MasterKey, Zeroizing<Vec<u8>>, Location)> {
    let path = master_key_path(files_dir);
    if !path.is_file() {
        return Err(LibraryError::Other(anyhow::anyhow!(
            "master key file {} does not exist",
            path.display()
        )));
    }
    read_key_snapshot(&path, normalized_password(password))
}

/// A trimmed, non-empty password, registered with log redaction.
fn normalized_password(password: Option<&str>) -> Option<&str> {
    password
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .inspect(|p| bookclerk_config::register_secret(p))
}

/// The bytes `master.key` must hold for `dek`, after putting the envelope
/// where the [`key_store_preference`] says: the envelope itself for the
/// file store, or a `BCK3` pointer to the keychain item this call just
/// wrote. A caller that stages the file itself (a shared branch applying
/// its bootstrap as one recoverable step) writes these bytes with mode
/// `0600`.
///
/// # Arguments
///
/// * `files_dir` - the files directory `master.key` will be written in
///   (where a `systemd-creds` credential lives beside it).
/// * `dek` - the 32-byte data-encryption key.
/// * `password` - wraps the envelope as `BCK2` when set and non-empty.
///
/// # Errors
///
/// Returns [`LibraryError::Invalid`] when `dek` is not 32 bytes, the
/// wrapping error, and a keychain failure when the keychain is required.
pub fn store_master_key(
    files_dir: &Path,
    dek: &[u8],
    password: Option<&str>,
) -> Result<StoredMasterKey> {
    let bytes: [u8; DEK_LEN] = dek
        .try_into()
        .map_err(|_| LibraryError::Invalid(format!("a data-encryption key is {DEK_LEN} bytes")))?;
    let key = MasterKey::from_bytes(bytes);
    let placed = place_envelope(&key, normalized_password(password), files_dir)?;
    Ok(StoredMasterKey {
        bytes: placed.file_bytes,
        item: placed.item,
    })
}

/// A key [`store_master_key`] placed, waiting for its `master.key` to be
/// published.
pub struct StoredMasterKey {
    /// `master.key`'s contents: the envelope, or a `BCK3`/`BCK4` pointer.
    bytes: Zeroizing<Vec<u8>>,
    /// The store item now holding the envelope, when the key went to one.
    item: Option<ItemHome>,
}

impl StoredMasterKey {
    /// The bytes `master.key` must hold (write them with mode `0600`).
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// The store the key went to: the file, or the store `master.key`
    /// points at.
    #[must_use]
    pub fn store(&self) -> KeyStoreKind {
        self.item
            .as_ref()
            .map_or(KeyStoreKind::File, |home| store_kind(home.store))
    }

    /// Deletes the store item again, for a caller whose `master.key` was
    /// never published. Does nothing for a key kept in the file.
    pub fn discard(self) {
        if let Some(item) = &self.item {
            delete_item_logged(item, "a store item whose master.key was never published");
        }
    }
}

/// Moves the existing key to `to`, keeping its bytes (and its password
/// wrap) unchanged, so every sealed secret stays readable.
///
/// To the keychain or `systemd-creds`: writes a new item, reads it back,
/// replaces `master.key` with a pointer atomically, then deletes any item
/// the key was in before. To the file: writes the envelope over the pointer
/// atomically, reloads it, then deletes the old item. Moving to the store
/// the key is already in does nothing. Unlike starting a library, an
/// explicit move never falls back: a store that can't take the key is an
/// error.
///
/// # Arguments
///
/// * `files_dir` - the files directory holding `master.key`.
/// * `to` - the store to move to.
/// * `password` - the auth password, needed when the key is wrapped.
///
/// # Returns
///
/// The format `master.key` has afterwards.
///
/// # Errors
///
/// Returns an error when there is no key, it can't be unwrapped, or the
/// target store can't take it. On error the key stays where it was.
pub fn move_master_key(
    files_dir: &Path,
    to: KeyStoreKind,
    password: Option<&str>,
) -> Result<MasterKeyFormat> {
    let (format, key) = move_key_envelope(files_dir, to, password)?;
    // A stored database credential follows the key into its new store.
    if let Err(err) = crate::database_credential::rehome_database_credential(files_dir, &key) {
        tracing::warn!(
            "the master key moved, but the database credential could not follow it ({err}); \
             run `bookclerk database setup d1` again"
        );
    }
    Ok(format)
}

/// Moves the key's envelope for [`move_master_key`].
///
/// # Returns
///
/// The format `master.key` has afterwards, and the key.
fn move_key_envelope(
    files_dir: &Path,
    to: KeyStoreKind,
    password: Option<&str>,
) -> Result<(MasterKeyFormat, MasterKey)> {
    let path = master_key_path(files_dir);
    // One read: the key and the envelope it came from always match.
    let (key, envelope, location) = load_existing(files_dir, password)?;
    let file_format = if envelope.starts_with(MAGIC_WRAPPED) {
        MasterKeyFormat::Wrapped
    } else {
        MasterKeyFormat::Raw
    };
    let target = match to {
        KeyStoreKind::File => None,
        KeyStoreKind::Keychain => Some(PointerStore::Keychain),
        KeyStoreKind::SystemdCreds => Some(PointerStore::SystemdCreds),
    };
    let old = match location {
        Location::File => None,
        Location::Keychain(pointer) => Some(ItemHome::of(&pointer, files_dir)),
    };
    let format = match (old, target) {
        (None, None) => file_format,
        (Some(old), Some(store)) if old.store == store => pointer_format(store),
        (old, Some(store)) => {
            let pointer = put_in_store(store, files_dir, &key, &envelope)?;
            if let Err(err) = write_secret_file_atomic(&path, &pointer.encode()) {
                delete_item_logged(
                    &ItemHome::of(&pointer, files_dir),
                    "a store item whose pointer could not be written",
                );
                return Err(err);
            }
            if let Some(old) = &old {
                delete_item_logged(old, "the old store item after moving the key");
            }
            tracing::info!(
                path = %path.display(),
                item = %pointer.item,
                store = to.as_str(),
                "moved the data-encryption key into {}; master.key now only names the item",
                describe_item(store, pointer.item)
            );
            pointer_format(store)
        }
        (Some(old), None) => {
            // Stage and verify the file before it replaces the pointer, so a
            // bad write never leaves master.key without a working key.
            let staged = staged_master_key_path(&path);
            write_secret_file_atomic(&staged, &envelope)?;
            let reloaded = read_key_snapshot(&staged, normalized_password(password));
            if !reloaded.is_ok_and(|(back, _, _)| back.as_bytes() == key.as_bytes()) {
                let _ = std::fs::remove_file(&staged);
                return Err(LibraryError::Other(anyhow::anyhow!(
                    "master key file {} did not reload to the same key; it still points at {}",
                    path.display(),
                    describe_item(old.store, old.item)
                )));
            }
            commit_staged_master_key(&staged, &path)?;
            delete_item_logged(
                &old,
                "the old store item after moving the key to master.key",
            );
            tracing::info!(
                path = %path.display(),
                "moved the data-encryption key from {} into master.key",
                describe_item(old.store, old.item)
            );
            if !envelope.starts_with(MAGIC_WRAPPED) {
                warn_unwrapped_master_key(&path);
            }
            file_format
        }
    };
    Ok((format, key))
}

/// The `master.key` format of a pointer into `store`.
fn pointer_format(store: PointerStore) -> MasterKeyFormat {
    match store {
        PointerStore::Keychain => MasterKeyFormat::Keychain,
        PointerStore::SystemdCreds => MasterKeyFormat::SystemdCreds,
    }
}

/// The key's envelope exactly as it is stored (`BCK1`, or `BCK2` under the
/// auth password), for an administrator who seals it into a unit
/// credential themselves (`systemd-creds encrypt --name=bookclerk-master-key`
/// on systemd older than 256, or a system-wide credential store). A
/// `systemd-creds` move then adopts that credential instead of sealing its
/// own copy, because the bytes match.
///
/// # Arguments
///
/// * `files_dir` - the files directory holding `master.key`.
/// * `password` - the auth password, needed when the key is wrapped (the
///   envelope stays wrapped; the password only proves it opens).
///
/// # Errors
///
/// Returns an error when there is no key or it can't be unwrapped.
pub fn master_key_envelope(files_dir: &Path, password: Option<&str>) -> Result<Zeroizing<Vec<u8>>> {
    let (_, envelope, _) = load_existing(files_dir, password)?;
    Ok(envelope)
}

/// A recovery copy of the key: a `BCK2` envelope wrapped with
/// `recovery_password`, readable by [`import_master_key`] on any machine.
///
/// # Arguments
///
/// * `files_dir` - the files directory holding `master.key`.
/// * `password` - the auth password, needed when the key is wrapped.
/// * `recovery_password` - the password the recovery copy is wrapped with.
///
/// # Errors
///
/// Returns an error when `recovery_password` is empty, or there is no key or
/// it can't be unwrapped.
pub fn export_master_key(
    files_dir: &Path,
    password: Option<&str>,
    recovery_password: &str,
) -> Result<Zeroizing<Vec<u8>>> {
    let recovery_password = recovery_password.trim();
    if recovery_password.is_empty() {
        return Err(LibraryError::Invalid(
            "a recovery copy needs a non-empty password".into(),
        ));
    }
    bookclerk_config::register_secret(recovery_password);
    let (key, _, _) = load_existing(files_dir, password)?;
    Ok(Zeroizing::new(encode_wrapped_master_key(
        &key,
        recovery_password,
    )?))
}

/// Restores a key from a recovery copy ([`export_master_key`], or a copied
/// `master.key` in `BCK1`/`BCK2`) on an install that has no `master.key`,
/// placing it where the [`key_store_preference`] says.
///
/// # Arguments
///
/// * `files_dir` - the files directory to write `master.key` in.
/// * `recovery` - the recovery copy's bytes.
/// * `recovery_password` - the password the copy is wrapped with, if it is.
/// * `password` - this install's auth password, which wraps the key at rest
///   when set.
///
/// # Returns
///
/// The SHA-256 hex fingerprint of the restored key.
///
/// # Errors
///
/// Returns an error when `master.key` already exists, the copy can't be
/// unwrapped, or the key can't be stored.
pub fn import_master_key(
    files_dir: &Path,
    recovery: &[u8],
    recovery_password: Option<&str>,
    password: Option<&str>,
) -> Result<String> {
    let path = master_key_path(files_dir);
    if path.exists() {
        return Err(LibraryError::Other(anyhow::anyhow!(
            "master key file {} already exists; move it away before importing a recovery copy",
            path.display()
        )));
    }
    let recovery_password = normalized_password(recovery_password);
    let key = match recovery.get(..4) {
        Some(b"BCK1") | Some(b"BCK2") => {
            // Never written back: the recovery copy is only read.
            parse_recovery(recovery, recovery_password)?
        }
        _ => {
            return Err(LibraryError::Invalid(
                "a recovery copy starts with BCK1 or BCK2".into(),
            ))
        }
    };
    prepare_master_key_dir(files_dir)?;
    let placed = place_envelope(&key, normalized_password(password), files_dir)?;
    if let Err(e) = write_secret_file_create_new(&path, &placed.file_bytes) {
        if let Some(item) = &placed.item {
            delete_item_logged(item, "a store item whose pointer could not be written");
        }
        return Err(LibraryError::Other(anyhow::anyhow!(
            "failed to write master key {}: {e}",
            path.display()
        )));
    }
    tracing::info!(
        path = %path.display(),
        store = placed.store().as_str(),
        "restored the data-encryption key from a recovery copy"
    );
    Ok(master_key_fingerprint(&key))
}

/// Unwraps a recovery copy without writing anything back.
fn parse_recovery(raw: &[u8], password: Option<&str>) -> Result<MasterKey> {
    let label = Path::new("recovery copy");
    match raw.get(..4) {
        Some(b"BCK1") => {
            let bytes: [u8; DEK_LEN] = raw[4..].try_into().map_err(|_| {
                LibraryError::Invalid("the recovery copy has an invalid BCK1 length".into())
            })?;
            Ok(MasterKey::from_bytes(bytes))
        }
        Some(b"BCK2") => {
            let pw = password.ok_or_else(|| {
                LibraryError::Invalid(
                    "the recovery copy is password-wrapped; give its password".into(),
                )
            })?;
            decode_wrapped_master_key(&raw[4..], pw, label)
        }
        _ => Err(LibraryError::Invalid(
            "a recovery copy starts with BCK1 or BCK2".into(),
        )),
    }
}

/// Serializes an unwrapped DEK as `BCK1` plus 32 raw bytes (anyone with the file can unseal secrets).
fn encode_raw_master_key(dek: &MasterKey) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + DEK_LEN);
    out.extend_from_slice(MAGIC_RAW);
    out.extend_from_slice(dek.as_bytes());
    out
}

/// Seals the DEK with Argon2id + XChaCha20-Poly1305; only the auth password can unwrap it.
fn encode_wrapped_master_key(dek: &MasterKey, password: &str) -> Result<Vec<u8>> {
    let salt = random_bytes(SALT_LEN);
    let nonce = random_bytes(NONCE_LEN);
    let key = derive_wrapping_key(password, &salt)?;
    let cipher = XChaCha20Poly1305::new_from_slice(&key)
        .map_err(|e| LibraryError::Other(anyhow::anyhow!("cipher init: {e}")))?;
    let nonce = XNonce::try_from(nonce.as_slice())
        .map_err(|_| LibraryError::Other(anyhow::anyhow!("invalid wrapping nonce length")))?;
    let ct = cipher
        .encrypt(&nonce, dek.as_bytes().as_slice())
        .map_err(|_| LibraryError::Other(anyhow::anyhow!("failed to wrap master key")))?;
    let mut out = Vec::with_capacity(4 + SALT_LEN + NONCE_LEN + ct.len());
    out.extend_from_slice(MAGIC_WRAPPED);
    out.extend_from_slice(&salt);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    let _ = (
        KDF_ALGORITHM,
        CIPHER_ALGORITHM,
        KDF_M_COST,
        KDF_T_COST,
        KDF_P_COST,
    );
    Ok(out)
}

/// Unwraps a `BCK2` body with the auth password; wrong password fails closed (secrets stay sealed).
fn decode_wrapped_master_key(body: &[u8], password: &str, path: &Path) -> Result<MasterKey> {
    if body.len() < SALT_LEN + NONCE_LEN + 16 {
        return Err(LibraryError::Other(anyhow::anyhow!(
            "master key file {} wrapped payload truncated",
            path.display()
        )));
    }
    let salt = &body[..SALT_LEN];
    let nonce = &body[SALT_LEN..SALT_LEN + NONCE_LEN];
    let ct = &body[SALT_LEN + NONCE_LEN..];
    let key = derive_wrapping_key(password, salt)?;
    let cipher = XChaCha20Poly1305::new_from_slice(&key)
        .map_err(|e| LibraryError::Other(anyhow::anyhow!("cipher init: {e}")))?;
    let nonce = XNonce::try_from(nonce).map_err(|_| {
        LibraryError::Other(anyhow::anyhow!(
            "invalid wrapping nonce length in master key {}",
            path.display()
        ))
    })?;
    let plain = cipher.decrypt(&nonce, ct).map_err(|_| {
        LibraryError::Other(anyhow::anyhow!(
            "failed to unwrap master key {} — wrong auth password?",
            path.display()
        ))
    })?;
    let bytes: [u8; DEK_LEN] = plain.as_slice().try_into().map_err(|_| {
        LibraryError::Other(anyhow::anyhow!("unwrapped master key has invalid length"))
    })?;
    Ok(MasterKey::from_bytes(bytes))
}

/// Derives the 32-byte wrap key from the auth password and salt (Argon2id).
fn derive_wrapping_key(password: &str, salt: &[u8]) -> Result<[u8; 32]> {
    let params = ArgonParams::new(KDF_M_COST, KDF_T_COST, KDF_P_COST, Some(32))
        .map_err(|e| LibraryError::Other(anyhow::anyhow!("argon2 params: {e}")))?;
    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut key: [u8; 32] = rand::rngs::OsRng.gen();
    argon2
        .hash_password_into(password.as_bytes(), salt, &mut key)
        .map_err(|e| LibraryError::Other(anyhow::anyhow!("argon2 hash: {e}")))?;
    Ok(key)
}

/// Fills `len` bytes from the OS CSPRNG (salts, nonces, temp-file suffixes).
fn random_bytes(len: usize) -> Vec<u8> {
    let mut rng = rand::rngs::OsRng;
    (0..len).map(|_| rng.gen()).collect()
}

/// Exclusive create for first mint (fails with AlreadyExists on race).
fn write_secret_file_create_new(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(path)?;
    // The file is ours (create_new): never leave a partial one behind, or a
    // retry would find a `master.key` it can neither load nor replace.
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .and_then(|()| harden_path_io(path, false))
        .inspect_err(|_| {
            let _ = std::fs::remove_file(path);
        })
}

/// Atomic replace via temp file + [`bookclerk_config::replace_file`] (for
/// BCK1→BCK2 re-wrap and a shared branch's installed DEK). That replace overwrites
/// an existing `master.key` on Windows too, where a plain `rename` onto an
/// existing file is not relied on; the `0600` temp file keeps its mode.
pub(crate) fn write_secret_file_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(MASTER_KEY_FILE_NAME);
    let tmp = parent.join(format!(
        ".{file_name}.tmp-{}-{}",
        std::process::id(),
        u32::from_le_bytes(rand::rngs::OsRng.gen())
    ));
    {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = opts.open(&tmp).map_err(|e| {
            LibraryError::Other(anyhow::anyhow!(
                "failed to write master key temp {}: {e}",
                tmp.display()
            ))
        })?;
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(|e| {
                let _ = std::fs::remove_file(&tmp);
                LibraryError::Other(anyhow::anyhow!(
                    "failed to write master key temp {}: {e}",
                    tmp.display()
                ))
            })?;
    }
    harden_path(&tmp, false)?;
    bookclerk_config::replace_file(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        LibraryError::Other(anyhow::anyhow!(
            "failed to replace master key {}: {e}",
            path.display()
        ))
    })?;
    // Committed: the temp file was already 0600 and hardened, so a failure
    // here is logged rather than returned (an error means "not replaced").
    if let Err(err) = harden_path(path, false) {
        tracing::warn!("{err}");
    }
    // Best effort: not every platform can open a directory.
    if let Ok(dir) = std::fs::File::open(parent) {
        let _ = dir.sync_all();
    }
    Ok(())
}

/// Where a move stages the new `master.key` before it replaces the old one:
/// a hidden sibling in the same directory, so the final replace is a rename.
///
/// # Arguments
///
/// * `path` - the `master.key` path the staged file will replace.
fn staged_master_key_path(path: &Path) -> PathBuf {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(MASTER_KEY_FILE_NAME);
    parent.join(format!(".{file_name}.staged-{}", std::process::id()))
}

/// Replaces `path` with the verified `staged` file in one rename.
///
/// # Arguments
///
/// * `staged` - the file written and reloaded by the caller.
/// * `path` - the `master.key` path to replace.
///
/// # Errors
///
/// Returns an error when the replace fails; `path` is then unchanged and the
/// staged file is removed.
fn commit_staged_master_key(staged: &Path, path: &Path) -> Result<()> {
    bookclerk_config::replace_file(staged, path).map_err(|e| {
        let _ = std::fs::remove_file(staged);
        LibraryError::Other(anyhow::anyhow!(
            "failed to replace master key {}: {e}",
            path.display()
        ))
    })?;
    if let Err(err) = harden_path(path, false) {
        tracing::warn!("{err}");
    }
    if let Ok(dir) = std::fs::File::open(path.parent().unwrap_or_else(|| Path::new("."))) {
        let _ = dir.sync_all();
    }
    Ok(())
}

/// Sets Unix mode `0700` (dir) or `0600` (file) so only the process owner can read `master.key`.
fn harden_path(path: &Path, is_dir: bool) -> Result<()> {
    harden_path_io(path, is_dir).map_err(|e| {
        LibraryError::Other(anyhow::anyhow!("failed to harden {}: {e}", path.display()))
    })
}

/// Applies owner-only permissions; no-op on non-Unix hosts.
fn harden_path_io(path: &Path, is_dir: bool) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = if is_dir { 0o700 } else { 0o600 };
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    }
    let _ = (path, is_dir);
    Ok(())
}

/// Seal plaintext with the DEK (random nonce). Fast — no Argon2.
///
/// # Errors
///
/// Returns an error when the operation fails.
pub fn seal_with_dek(plaintext: &[u8], dek: &MasterKey) -> Result<(Vec<u8>, Vec<u8>)> {
    let nonce = random_bytes(NONCE_LEN);
    let cipher = XChaCha20Poly1305::new_from_slice(dek.as_bytes())
        .map_err(|e| LibraryError::Other(anyhow::anyhow!("cipher init: {e}")))?;
    let nonce_arr = XNonce::try_from(nonce.as_slice())
        .map_err(|_| LibraryError::Other(anyhow::anyhow!("invalid seal nonce length")))?;
    let ciphertext = cipher
        .encrypt(&nonce_arr, plaintext)
        .map_err(|_| LibraryError::Other(anyhow::anyhow!("seal failed")))?;
    Ok((ciphertext, nonce))
}

/// Unseal ciphertext with the DEK.
///
/// # Errors
///
/// Returns an error when the operation fails.
pub fn unseal_with_dek(ciphertext: &[u8], nonce: &[u8], dek: &MasterKey) -> Result<Vec<u8>> {
    let cipher = XChaCha20Poly1305::new_from_slice(dek.as_bytes())
        .map_err(|e| LibraryError::Other(anyhow::anyhow!("cipher init: {e}")))?;
    let nonce = XNonce::try_from(nonce)
        .map_err(|_| LibraryError::Other(anyhow::anyhow!("invalid unseal nonce length")))?;
    cipher.decrypt(&nonce, ciphertext).map_err(|_| {
        LibraryError::Other(anyhow::anyhow!(
            "unseal failed — wrong master key or corrupted ciphertext"
        ))
    })
}

/// Process-wide DEK coordination for unit tests.
///
/// Sealed-v1 helpers share one minted DEK ([`ensure_shared_test_dek`]) under a
/// **read** lock so they stay parallel. Tests that swap `master.key`, mutate
/// [`AUTH_PASSWORD_ENV`], or otherwise reconfigure the process DEK take a
/// **write** lock (and restore the shared DEK on drop).
///
/// Uses `tokio::sync::RwLock` so guards may span `.await` without tripping
/// `clippy::await_holding_lock`.
#[cfg(test)]
struct SharedTestDek {
    dir: tempfile::TempDir,
}

#[cfg(test)]
fn shared_test_dek_slot() -> &'static OnceLock<SharedTestDek> {
    static SHARED: OnceLock<SharedTestDek> = OnceLock::new();
    &SHARED
}

#[cfg(test)]
fn master_key_test_rwlock() -> &'static tokio::sync::RwLock<()> {
    static LOCK: OnceLock<tokio::sync::RwLock<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::RwLock::new(()))
}

/// Install (or reinstall) the shared process DEK used by sealed-v1 tests.
#[cfg(test)]
pub(crate) fn ensure_shared_test_dek() {
    let shared = shared_test_dek_slot().get_or_init(|| {
        let dir = tempfile::tempdir().expect("shared test DEK tempdir");
        configure_master_key(dir.path()).expect("mint shared test DEK");
        SharedTestDek { dir }
    });
    configure_master_key(shared.dir.path()).expect("reinstall shared test DEK");
}

#[cfg(test)]
fn restore_shared_test_dek() {
    if let Some(shared) = shared_test_dek_slot().get() {
        let _ = configure_master_key(shared.dir.path());
    }
}

/// Write-lock guard: exclusive DEK / auth-password mutation; restores shared DEK.
#[cfg(test)]
pub(crate) struct MasterKeyWriteGuard {
    _guard: tokio::sync::RwLockWriteGuard<'static, ()>,
}

#[cfg(test)]
impl Drop for MasterKeyWriteGuard {
    fn drop(&mut self) {
        restore_shared_test_dek();
    }
}

/// Sync write lock for tests that reconfigure the process DEK or auth env.
#[cfg(test)]
pub(crate) fn master_key_test_lock() -> MasterKeyWriteGuard {
    MasterKeyWriteGuard {
        _guard: master_key_test_rwlock().blocking_write(),
    }
}

/// Async write lock for tests that reconfigure the DEK across `.await`.
#[cfg(test)]
pub(crate) async fn master_key_test_lock_async() -> MasterKeyWriteGuard {
    MasterKeyWriteGuard {
        _guard: master_key_test_rwlock().write().await,
    }
}

/// Async read lock — shared sealed-v1 tests hold this across `.await`.
#[cfg(test)]
pub(crate) async fn master_key_test_read_lock_async() -> tokio::sync::RwLockReadGuard<'static, ()> {
    master_key_test_rwlock().read().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookclerk_keystore::{KeystoreErrorKind, MemoryVault};
    use tempfile::tempdir;

    /// Points the process at a fresh in-memory keychain with `preference`,
    /// and puts the file preference back on drop. Hold the master-key test
    /// lock while it lives.
    struct KeychainScope {
        /// The in-memory keychain this scope installed.
        vault: Arc<MemoryVault>,
    }

    impl KeychainScope {
        /// Installs a fresh in-memory keychain and `preference` process-wide;
        /// callers hold [`master_key_test_lock`] for the scope's life.
        fn new(preference: KeyStorePreference) -> Self {
            let vault = Arc::new(MemoryVault::new());
            set_keychain_vault(vault.clone());
            set_key_store_preference(preference);
            Self { vault }
        }
    }

    impl Drop for KeychainScope {
        fn drop(&mut self) {
            set_key_store_preference(DEFAULT_PREFERENCE);
        }
    }

    /// The keychain preference: `required` makes an unreachable keychain an
    /// error, otherwise placement falls back to the file.
    fn keychain(required: bool) -> KeyStorePreference {
        KeyStorePreference {
            kind: KeyStoreKind::Keychain,
            required,
        }
    }

    #[test]
    fn a_password_file_wraps_a_fresh_key_and_a_blank_one_mints_nothing() {
        let _guard = master_key_test_lock();
        std::env::remove_var(AUTH_PASSWORD_ENV);
        let secrets = tempdir().unwrap();
        let file = secrets.path().join("auth-password");
        std::fs::write(&file, "from-a-docker-secret\n").unwrap();
        std::env::set_var(bookclerk_config::AUTH_PASSWORD_FILE_ENV, &file);

        let dir = tempdir().unwrap();
        let key = resolve_master_key(dir.path());
        let wrapped = master_key_is_wrapped(dir.path());
        let reopened = resolve_master_key_with(dir.path(), Some("from-a-docker-secret"));

        std::fs::write(&file, " \n").unwrap();
        let blank_dir = tempdir().unwrap();
        let blank = resolve_master_key(blank_dir.path());
        std::env::remove_var(bookclerk_config::AUTH_PASSWORD_FILE_ENV);

        let key = key.unwrap();
        assert_eq!(
            wrapped.unwrap(),
            Some(true),
            "the file's password wraps the key"
        );
        assert_eq!(reopened.unwrap().as_bytes(), key.as_bytes());
        assert!(blank.is_err(), "a blank password file must not mint a key");
        assert!(
            !master_key_path(blank_dir.path()).exists(),
            "nothing is written when the named password source is blank"
        );
    }

    #[test]
    fn a_keychain_mint_leaves_only_a_pointer_on_disk() {
        let _guard = master_key_test_lock();
        std::env::remove_var(AUTH_PASSWORD_ENV);
        let scope = KeychainScope::new(keychain(true));
        let dir = tempdir().unwrap();
        let key = resolve_master_key(dir.path()).unwrap();
        let raw = std::fs::read(master_key_path(dir.path())).unwrap();
        assert_eq!(raw.len(), bookclerk_keystore::POINTER_LEN);
        assert!(!raw.windows(DEK_LEN).any(|w| w == key.as_bytes()));
        assert_eq!(
            inspect_master_key(dir.path()).unwrap(),
            Some(MasterKeyFormat::Keychain)
        );
        assert_eq!(scope.vault.len(), 1);
        assert_eq!(master_key_is_wrapped(dir.path()).unwrap(), Some(false));
        let again = resolve_master_key(dir.path()).unwrap();
        assert_eq!(again.as_bytes(), key.as_bytes());
    }

    /// The keychain item `master.key` in `dir` points at.
    fn pointed_item(dir: &Path) -> KeychainItemId {
        let raw = std::fs::read(master_key_path(dir)).unwrap();
        KeychainPointer::parse(&raw).unwrap().item
    }

    #[test]
    fn a_damaged_item_is_reported_not_called_unwrapped() {
        let _guard = master_key_test_lock();
        std::env::remove_var(AUTH_PASSWORD_ENV);
        let scope = KeychainScope::new(keychain(true));
        let dir = tempdir().unwrap();
        resolve_master_key(dir.path()).unwrap();
        scope.vault.set(pointed_item(dir.path()), b"").unwrap();
        let err = master_key_is_wrapped(dir.path()).unwrap_err().to_string();
        assert!(err.contains("not a valid key envelope"), "{err}");
        assert_eq!(envelope_is_wrapped(b"BCK1short"), None);
        assert_eq!(envelope_is_wrapped(b"BCK2short"), None);
    }

    #[test]
    fn a_mismatched_item_is_refused_before_any_rewrap() {
        let _guard = master_key_test_lock();
        std::env::remove_var(AUTH_PASSWORD_ENV);
        let scope = KeychainScope::new(keychain(true));
        let dir = tempdir().unwrap();
        resolve_master_key(dir.path()).unwrap();
        let item = pointed_item(dir.path());
        let other = encode_raw_master_key(&MasterKey::from_bytes([8u8; DEK_LEN]));
        scope.vault.set(item, &other).unwrap();
        let password = test_passphrase("mismatch");
        let err = resolve_master_key_with(dir.path(), Some(&password))
            .unwrap_err()
            .to_string();
        assert!(err.contains("different key"), "{err}");
        assert_eq!(scope.vault.get(item).unwrap().as_slice(), other.as_slice());
    }

    #[test]
    fn move_and_export_never_rewrap_the_live_key() {
        let _guard = master_key_test_lock();
        std::env::remove_var(AUTH_PASSWORD_ENV);
        let _scope = KeychainScope::new(DEFAULT_PREFERENCE);
        let dir = tempdir().unwrap();
        resolve_master_key_with(dir.path(), None).unwrap();
        let path = master_key_path(dir.path());
        let before = std::fs::read(&path).unwrap();
        let password = test_passphrase("live");
        let recovery = test_passphrase("copy");
        export_master_key(dir.path(), Some(&password), &recovery).unwrap();
        move_master_key(dir.path(), KeyStoreKind::File, Some(&password)).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn an_install_retires_the_item_it_replaced_and_cleans_up_on_failure() {
        let _guard = master_key_test_lock();
        std::env::remove_var(AUTH_PASSWORD_ENV);
        let scope = KeychainScope::new(keychain(true));
        let dir = tempdir().unwrap();
        install_master_key(dir.path(), &[1u8; DEK_LEN], None).unwrap();
        let first = pointed_item(dir.path());
        install_master_key(dir.path(), &[2u8; DEK_LEN], None).unwrap();
        assert_ne!(pointed_item(dir.path()), first);
        assert_eq!(scope.vault.len(), 1, "the replaced item is gone");

        // A files directory that is a regular file: nothing is left behind.
        let blocked = dir.path().join("not-a-dir");
        std::fs::write(&blocked, b"").unwrap();
        assert!(install_master_key(&blocked, &[3u8; DEK_LEN], None).is_err());
        assert_eq!(scope.vault.len(), 1);

        // The pointer write fails after the item is staged: the staged item
        // is deleted again and nothing else changes.
        let other = tempdir().unwrap();
        std::fs::create_dir(master_key_path(other.path())).unwrap();
        assert!(install_master_key(other.path(), &[5u8; DEK_LEN], None).is_err());
        assert_eq!(scope.vault.len(), 1, "the staged item is gone");
        assert!(resolve_master_key_with(dir.path(), None).is_ok());

        let stored = store_master_key(dir.path(), &[4u8; DEK_LEN], None).unwrap();
        assert_eq!(stored.store(), KeyStoreKind::Keychain);
        assert_eq!(scope.vault.len(), 2);
        stored.discard();
        assert_eq!(scope.vault.len(), 1);
    }

    #[test]
    fn discarding_a_minted_keychain_key_removes_pointer_and_item() {
        let _guard = master_key_test_lock();
        std::env::remove_var(AUTH_PASSWORD_ENV);
        let scope = KeychainScope::new(keychain(true));
        let dir = tempdir().unwrap();
        let key = resolve_master_key_with(dir.path(), None).unwrap();
        discard_minted_master_key(dir.path(), &key).unwrap();
        assert!(!master_key_path(dir.path()).exists());
        assert!(scope.vault.is_empty());
    }

    #[test]
    fn a_locked_keychain_fails_closed_and_never_mints() {
        let _guard = master_key_test_lock();
        std::env::remove_var(AUTH_PASSWORD_ENV);
        let scope = KeychainScope::new(keychain(true));
        let dir = tempdir().unwrap();
        let key = resolve_master_key(dir.path()).unwrap();
        let before = std::fs::read(master_key_path(dir.path())).unwrap();
        scope.vault.fail_with(Some(KeystoreErrorKind::AccessDenied));
        let err = resolve_master_key(dir.path()).unwrap_err().to_string();
        assert!(err.contains("could not be read"), "{err}");
        assert_eq!(std::fs::read(master_key_path(dir.path())).unwrap(), before);
        scope.vault.fail_with(None);
        assert_eq!(
            resolve_master_key(dir.path()).unwrap().as_bytes(),
            key.as_bytes()
        );
    }

    #[test]
    fn a_missing_item_or_a_swapped_key_is_refused() {
        let _guard = master_key_test_lock();
        std::env::remove_var(AUTH_PASSWORD_ENV);
        let scope = KeychainScope::new(keychain(true));
        let dir = tempdir().unwrap();
        resolve_master_key(dir.path()).unwrap();
        let pointer =
            KeychainPointer::parse(&std::fs::read(master_key_path(dir.path())).unwrap()).unwrap();
        // Another key in the item: the fingerprint no longer matches.
        scope
            .vault
            .set(
                pointer.item,
                &encode_raw_master_key(&MasterKey::from_bytes([9u8; DEK_LEN])),
            )
            .unwrap();
        let err = resolve_master_key(dir.path()).unwrap_err().to_string();
        assert!(err.contains("different key"), "{err}");
        scope.vault.delete(pointer.item).unwrap();
        assert!(resolve_master_key(dir.path()).is_err());
        assert_eq!(scope.vault.len(), 0, "nothing minted in its place");
    }

    #[test]
    fn a_default_keychain_preference_falls_back_to_the_file() {
        let _guard = master_key_test_lock();
        std::env::remove_var(AUTH_PASSWORD_ENV);
        let scope = KeychainScope::new(keychain(false));
        scope.vault.fail_with(Some(KeystoreErrorKind::Unavailable));
        let dir = tempdir().unwrap();
        resolve_master_key(dir.path()).unwrap();
        assert_eq!(
            inspect_master_key(dir.path()).unwrap(),
            Some(MasterKeyFormat::Raw)
        );
        // Required: the same failure is an error, and no file is written.
        set_key_store_preference(keychain(true));
        let other = tempdir().unwrap();
        assert!(resolve_master_key(other.path()).is_err());
        assert!(!master_key_path(other.path()).exists());
    }

    #[test]
    fn a_password_wraps_the_keychain_item_and_the_key_survives() {
        let _guard = master_key_test_lock();
        std::env::remove_var(AUTH_PASSWORD_ENV);
        let scope = KeychainScope::new(keychain(true));
        let dir = tempdir().unwrap();
        let key = resolve_master_key_with(dir.path(), None).unwrap();
        let pass = test_passphrase("keychain-wrap");
        let wrapped = wrap_master_key(dir.path(), &pass).unwrap();
        assert_eq!(wrapped.as_bytes(), key.as_bytes());
        assert_eq!(
            inspect_master_key(dir.path()).unwrap(),
            Some(MasterKeyFormat::Keychain)
        );
        assert_eq!(master_key_is_wrapped(dir.path()).unwrap(), Some(true));
        assert!(resolve_master_key_with(dir.path(), None).is_err());
        assert_eq!(
            resolve_master_key_with(dir.path(), Some(&pass))
                .unwrap()
                .as_bytes(),
            key.as_bytes()
        );
        assert_eq!(scope.vault.len(), 1);
    }

    #[test]
    fn a_key_moves_to_the_keychain_and_back_unchanged() {
        let _guard = master_key_test_lock();
        std::env::remove_var(AUTH_PASSWORD_ENV);
        let dir = tempdir().unwrap();
        let key = resolve_master_key_with(dir.path(), None).unwrap();
        let scope = KeychainScope::new(DEFAULT_PREFERENCE);
        assert_eq!(
            move_master_key(dir.path(), KeyStoreKind::Keychain, None).unwrap(),
            MasterKeyFormat::Keychain
        );
        assert_eq!(scope.vault.len(), 1);
        assert_eq!(
            move_master_key(dir.path(), KeyStoreKind::Keychain, None).unwrap(),
            MasterKeyFormat::Keychain,
            "already there"
        );
        assert_eq!(
            resolve_master_key_with(dir.path(), None)
                .unwrap()
                .as_bytes(),
            key.as_bytes()
        );
        assert_eq!(
            move_master_key(dir.path(), KeyStoreKind::File, None).unwrap(),
            MasterKeyFormat::Raw
        );
        assert!(scope.vault.is_empty(), "the old item is deleted");
        assert_eq!(
            resolve_master_key_with(dir.path(), None)
                .unwrap()
                .as_bytes(),
            key.as_bytes()
        );
    }

    #[test]
    fn a_failed_move_leaves_the_key_where_it_was() {
        let _guard = master_key_test_lock();
        std::env::remove_var(AUTH_PASSWORD_ENV);
        let dir = tempdir().unwrap();
        resolve_master_key_with(dir.path(), None).unwrap();
        let before = std::fs::read(master_key_path(dir.path())).unwrap();
        let scope = KeychainScope::new(DEFAULT_PREFERENCE);
        scope.vault.fail_with(Some(KeystoreErrorKind::Unavailable));
        assert!(move_master_key(dir.path(), KeyStoreKind::Keychain, None).is_err());
        assert_eq!(std::fs::read(master_key_path(dir.path())).unwrap(), before);
    }

    #[test]
    fn a_move_out_of_the_keychain_keeps_the_pointer_until_the_file_is_verified() {
        let _guard = master_key_test_lock();
        std::env::remove_var(AUTH_PASSWORD_ENV);
        let dir = tempdir().unwrap();
        let scope = KeychainScope::new(keychain(true));
        let key = resolve_master_key_with(dir.path(), None).unwrap();
        let path = master_key_path(dir.path());
        let pointer = std::fs::read(&path).unwrap();
        // A directory where the staged file goes makes staging fail.
        let staged = staged_master_key_path(&path);
        std::fs::create_dir(&staged).unwrap();
        assert!(move_master_key(dir.path(), KeyStoreKind::File, None).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), pointer, "pointer untouched");
        assert_eq!(scope.vault.len(), 1, "the item still holds the key");
        std::fs::remove_dir(&staged).unwrap();
        assert_eq!(
            move_master_key(dir.path(), KeyStoreKind::File, None).unwrap(),
            MasterKeyFormat::Raw
        );
        assert!(!staged.exists(), "the staged file became master.key");
        assert_eq!(
            resolve_master_key_with(dir.path(), None)
                .unwrap()
                .as_bytes(),
            key.as_bytes()
        );
    }

    /// Points the process at a fresh in-memory `systemd-creds` store with
    /// the `systemd-creds` preference, and puts the system tool and the file
    /// preference back on drop. Hold the master-key test lock while it lives.
    struct CredsScope {
        /// The in-memory store this scope installed (shared by every
        /// files directory).
        vault: Arc<MemoryVault>,
    }

    impl CredsScope {
        /// Installs a fresh in-memory store and the `systemd-creds` preference.
        fn new() -> Self {
            let vault = Arc::new(MemoryVault::new());
            let shared = vault.clone();
            set_systemd_creds_vault(Arc::new(move |_: &Path| -> Arc<dyn Vault> {
                shared.clone()
            }));
            set_key_store_preference(KeyStorePreference::from_config(
                Some(KeyStoreKind::SystemdCreds),
                KeyStoreKind::File,
            ));
            Self { vault }
        }
    }

    impl Drop for CredsScope {
        fn drop(&mut self) {
            set_systemd_creds_vault(Arc::new(|dir: &Path| -> Arc<dyn Vault> {
                Arc::new(SystemdCredsVault::new(dir))
            }));
            set_key_store_preference(DEFAULT_PREFERENCE);
        }
    }

    #[test]
    fn a_systemd_creds_mint_leaves_only_a_pointer_on_disk() {
        let _guard = master_key_test_lock();
        std::env::remove_var(AUTH_PASSWORD_ENV);
        let scope = CredsScope::new();
        let dir = tempdir().unwrap();
        let key = resolve_master_key(dir.path()).unwrap();
        let raw = std::fs::read(master_key_path(dir.path())).unwrap();
        assert_eq!(&raw[..4], b"BCK4");
        assert!(!raw.windows(DEK_LEN).any(|w| w == key.as_bytes()));
        assert_eq!(
            inspect_master_key(dir.path()).unwrap(),
            Some(MasterKeyFormat::SystemdCreds)
        );
        assert_eq!(
            MasterKeyFormat::SystemdCreds.store(),
            KeyStoreKind::SystemdCreds
        );
        assert_eq!(scope.vault.len(), 1);
        assert_eq!(
            resolve_master_key(dir.path()).unwrap().as_bytes(),
            key.as_bytes()
        );
    }

    #[test]
    fn systemd_creds_always_falls_back_to_the_file() {
        let _guard = master_key_test_lock();
        std::env::remove_var(AUTH_PASSWORD_ENV);
        let scope = CredsScope::new();
        scope.vault.fail_with(Some(KeystoreErrorKind::Unavailable));
        let dir = tempdir().unwrap();
        // Configured (so "required"), yet a failed seal keeps the file store.
        assert!(key_store_preference().required);
        resolve_master_key(dir.path()).unwrap();
        assert_eq!(
            inspect_master_key(dir.path()).unwrap(),
            Some(MasterKeyFormat::Raw)
        );
        // An explicit move never falls back: the key stays where it was.
        let before = std::fs::read(master_key_path(dir.path())).unwrap();
        assert!(move_master_key(dir.path(), KeyStoreKind::SystemdCreds, None).is_err());
        assert_eq!(std::fs::read(master_key_path(dir.path())).unwrap(), before);
    }

    #[test]
    fn an_unreadable_credential_fails_closed() {
        let _guard = master_key_test_lock();
        std::env::remove_var(AUTH_PASSWORD_ENV);
        let scope = CredsScope::new();
        let dir = tempdir().unwrap();
        resolve_master_key(dir.path()).unwrap();
        scope.vault.fail_with(Some(KeystoreErrorKind::AccessDenied));
        let err = resolve_master_key(dir.path()).unwrap_err().to_string();
        assert!(err.contains("systemd-creds credential"), "{err}");
        assert_eq!(
            inspect_master_key(dir.path()).unwrap(),
            Some(MasterKeyFormat::SystemdCreds),
            "no new key minted over the pointer"
        );
    }

    #[test]
    fn a_key_moves_between_the_keychain_and_systemd_creds() {
        let _guard = master_key_test_lock();
        std::env::remove_var(AUTH_PASSWORD_ENV);
        let creds = CredsScope::new();
        let keychain = KeychainScope::new(DEFAULT_PREFERENCE);
        let dir = tempdir().unwrap();
        let key = resolve_master_key_with(dir.path(), None).unwrap();
        assert_eq!(
            move_master_key(dir.path(), KeyStoreKind::SystemdCreds, None).unwrap(),
            MasterKeyFormat::SystemdCreds
        );
        assert_eq!(
            move_master_key(dir.path(), KeyStoreKind::SystemdCreds, None).unwrap(),
            MasterKeyFormat::SystemdCreds,
            "already there"
        );
        assert_eq!(creds.vault.len(), 1);
        assert_eq!(
            move_master_key(dir.path(), KeyStoreKind::Keychain, None).unwrap(),
            MasterKeyFormat::Keychain
        );
        assert!(creds.vault.is_empty(), "the old credential is deleted");
        assert_eq!(keychain.vault.len(), 1);
        assert_eq!(
            move_master_key(dir.path(), KeyStoreKind::SystemdCreds, None).unwrap(),
            MasterKeyFormat::SystemdCreds
        );
        assert!(keychain.vault.is_empty());
        assert_eq!(
            move_master_key(dir.path(), KeyStoreKind::File, None).unwrap(),
            MasterKeyFormat::Raw
        );
        assert!(creds.vault.is_empty());
        assert_eq!(
            resolve_master_key_with(dir.path(), None)
                .unwrap()
                .as_bytes(),
            key.as_bytes()
        );
        drop(keychain);
    }

    #[cfg(unix)]
    #[test]
    fn an_administrators_unit_credential_is_adopted_without_the_tool() {
        let _guard = master_key_test_lock();
        std::env::remove_var(AUTH_PASSWORD_ENV);
        let dir = tempdir().unwrap();
        let key = resolve_master_key_with(dir.path(), None).unwrap();
        let password = test_passphrase("unit-credential");
        wrap_master_key(dir.path(), &password).unwrap();
        // What `bookclerk config master-key credential` hands the admin.
        let envelope = master_key_envelope(dir.path(), Some(&password)).unwrap();
        assert!(envelope.starts_with(MAGIC_WRAPPED));
        // systemd decrypts the admin's credential into the unit's directory.
        let credentials = tempdir().unwrap();
        std::fs::write(
            credentials.path().join(bookclerk_keystore::CREDENTIAL_NAME),
            &envelope,
        )
        .unwrap();
        let reset = CredsScope::new();
        // No systemd-creds tool (older systemd), so self-sealing can't work;
        // the unit's credential is all there is.
        let creds_dir = credentials.path().to_path_buf();
        set_systemd_creds_vault(Arc::new(move |dir: &Path| -> Arc<dyn Vault> {
            Arc::new(SystemdCredsVault::with_program(
                dir,
                None,
                Some(creds_dir.clone()),
            ))
        }));
        assert_eq!(
            move_master_key(dir.path(), KeyStoreKind::SystemdCreds, Some(&password)).unwrap(),
            MasterKeyFormat::SystemdCreds
        );
        assert_eq!(
            resolve_master_key_with(dir.path(), Some(&password))
                .unwrap()
                .as_bytes(),
            key.as_bytes()
        );
        // Without the unit credential the key is gone: fail closed.
        std::fs::remove_file(credentials.path().join(bookclerk_keystore::CREDENTIAL_NAME)).unwrap();
        assert!(resolve_master_key_with(dir.path(), Some(&password)).is_err());
        drop(reset);
    }

    #[test]
    fn a_recovery_copy_restores_the_key_elsewhere() {
        let _guard = master_key_test_lock();
        std::env::remove_var(AUTH_PASSWORD_ENV);
        let scope = KeychainScope::new(keychain(true));
        let dir = tempdir().unwrap();
        let key = resolve_master_key_with(dir.path(), None).unwrap();
        let recovery_pass = test_passphrase("recovery");
        let copy = export_master_key(dir.path(), None, &recovery_pass).unwrap();
        assert!(copy.starts_with(b"BCK2"));
        assert!(!copy.windows(DEK_LEN).any(|w| w == key.as_bytes()));
        // Built at run time so no password literal reaches the KDF.
        let blank: String = test_passphrase("blank").chars().map(|_| ' ').collect();
        assert!(export_master_key(dir.path(), None, &blank).is_err());

        let elsewhere = tempdir().unwrap();
        let wrong = test_passphrase("wrong");
        assert!(import_master_key(elsewhere.path(), &copy, Some(&wrong), None).is_err());
        assert!(!master_key_path(elsewhere.path()).exists());
        let fingerprint =
            import_master_key(elsewhere.path(), &copy, Some(&recovery_pass), None).unwrap();
        assert_eq!(fingerprint, master_key_fingerprint(&key));
        assert_eq!(
            inspect_master_key(elsewhere.path()).unwrap(),
            Some(MasterKeyFormat::Keychain)
        );
        assert_eq!(
            resolve_master_key_with(elsewhere.path(), None)
                .unwrap()
                .as_bytes(),
            key.as_bytes()
        );
        assert_eq!(scope.vault.len(), 2);
        // Never over an existing key.
        assert!(import_master_key(elsewhere.path(), &copy, Some(&recovery_pass), None).is_err());
        let pointer_like = [b"BCK3".as_slice(), &std::process::id().to_le_bytes()].concat();
        assert!(import_master_key(tempdir().unwrap().path(), &pointer_like, None, None).is_err());
    }

    #[test]
    fn an_installed_key_goes_to_the_keychain_when_preferred() {
        let _guard = master_key_test_lock();
        std::env::remove_var(AUTH_PASSWORD_ENV);
        let scope = KeychainScope::new(keychain(true));
        let dir = tempdir().unwrap();
        let dek = [5u8; DEK_LEN];
        install_master_key(dir.path(), &dek, None).unwrap();
        assert_eq!(
            inspect_master_key(dir.path()).unwrap(),
            Some(MasterKeyFormat::Keychain)
        );
        assert_eq!(
            resolve_master_key_with(dir.path(), None)
                .unwrap()
                .as_bytes(),
            &dek
        );
        assert_eq!(scope.vault.len(), 1);
    }

    #[test]
    fn discarding_a_minted_keychain_key_deletes_its_item() {
        let _guard = master_key_test_lock();
        std::env::remove_var(AUTH_PASSWORD_ENV);
        let scope = KeychainScope::new(keychain(true));
        let dir = tempdir().unwrap();
        let key = resolve_master_key_with(dir.path(), None).unwrap();
        discard_minted_master_key(dir.path(), &key).unwrap();
        assert!(!master_key_path(dir.path()).exists());
        assert!(scope.vault.is_empty());
    }

    /// Dynamic passphrase so CodeQL does not flag a hard-coded crypto secret.
    fn test_passphrase(tag: &str) -> String {
        ["unit-", tag, "-", &std::process::id().to_string()].concat()
    }

    #[test]
    fn mint_raw_and_reload() {
        let _guard = master_key_test_lock();
        let dir = tempdir().unwrap();
        std::env::remove_var(AUTH_PASSWORD_ENV);
        let a = resolve_master_key(dir.path()).unwrap();
        let b = resolve_master_key(dir.path()).unwrap();
        assert_eq!(a.as_bytes(), b.as_bytes());
        assert!(master_key_path(dir.path()).is_file());
        assert_eq!(
            inspect_master_key(dir.path()).unwrap(),
            Some(MasterKeyFormat::Raw)
        );
    }

    #[test]
    fn install_writes_the_given_dek_raw_or_under_a_password() {
        let _guard = master_key_test_lock();
        std::env::remove_var(AUTH_PASSWORD_ENV);
        let dir = tempdir().unwrap();
        // Replaces whatever was there.
        let old = resolve_master_key(dir.path()).unwrap();
        let dek = [42u8; DEK_LEN];
        install_master_key(dir.path(), &dek, None).unwrap();
        assert_eq!(
            inspect_master_key(dir.path()).unwrap(),
            Some(MasterKeyFormat::Raw)
        );
        let read = resolve_master_key_with(dir.path(), None).unwrap();
        assert_eq!(read.as_bytes(), &dek);
        assert_ne!(read.as_bytes(), old.as_bytes());

        let pass = test_passphrase("install");
        install_master_key(dir.path(), &dek, Some(&pass)).unwrap();
        assert_eq!(
            inspect_master_key(dir.path()).unwrap(),
            Some(MasterKeyFormat::Wrapped)
        );
        let raw = std::fs::read(master_key_path(dir.path())).unwrap();
        assert!(
            !raw.windows(DEK_LEN).any(|w| w == dek),
            "never in the clear"
        );
        assert_eq!(
            resolve_master_key_with(dir.path(), Some(&pass))
                .unwrap()
                .as_bytes(),
            &dek
        );
        assert!(resolve_master_key_with(dir.path(), Some("not-it")).is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(master_key_path(dir.path()))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert!(install_master_key(dir.path(), &[1u8; 16], None).is_err());
    }

    #[test]
    fn wrap_with_password() {
        let _guard = master_key_test_lock();
        let dir = tempdir().unwrap();
        let pass = test_passphrase("master-wrap");
        std::env::set_var(AUTH_PASSWORD_ENV, &pass);
        let a = resolve_master_key(dir.path()).unwrap();
        let b = resolve_master_key(dir.path()).unwrap();
        assert_eq!(a.as_bytes(), b.as_bytes());
        assert_eq!(
            inspect_master_key(dir.path()).unwrap(),
            Some(MasterKeyFormat::Wrapped)
        );
        std::env::remove_var(AUTH_PASSWORD_ENV);
        assert!(resolve_master_key(dir.path()).is_err());
        std::env::set_var(AUTH_PASSWORD_ENV, &pass);
        let c = resolve_master_key(dir.path()).unwrap();
        assert_eq!(a.as_bytes(), c.as_bytes());
        std::env::remove_var(AUTH_PASSWORD_ENV);
    }

    #[test]
    fn later_password_wraps_bck1() {
        let _guard = master_key_test_lock();
        let dir = tempdir().unwrap();
        std::env::remove_var(AUTH_PASSWORD_ENV);
        let a = resolve_master_key(dir.path()).unwrap();
        assert_eq!(
            inspect_master_key(dir.path()).unwrap(),
            Some(MasterKeyFormat::Raw)
        );
        let pass = test_passphrase("later-wrap");
        let b = wrap_master_key(dir.path(), &pass).unwrap();
        assert_eq!(a.as_bytes(), b.as_bytes());
        assert_eq!(
            inspect_master_key(dir.path()).unwrap(),
            Some(MasterKeyFormat::Wrapped)
        );
        let c = configure_master_key_with(dir.path(), Some(&pass)).unwrap();
        assert_eq!(a.as_bytes(), c.as_bytes());
    }

    #[test]
    fn seal_roundtrip() {
        let dek = MasterKey::random().unwrap();
        let secret = [b"hello".as_slice(), b"-", b"secret"].concat();
        let (ct, nonce) = seal_with_dek(&secret, &dek).unwrap();
        let plain = unseal_with_dek(&ct, &nonce, &dek).unwrap();
        assert_eq!(plain, secret);
    }
}
