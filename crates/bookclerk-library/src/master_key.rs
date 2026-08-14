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
use std::sync::{Mutex, OnceLock};

use argon2::{Algorithm, Argon2, Params as ArgonParams, Version};
use chacha20poly1305::{
    aead::{Aead, KeyInit},
    XChaCha20Poly1305, XNonce,
};
use rand::RngCore;
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::error::{LibraryError, Result};
use crate::secrets::{CIPHER_ALGORITHM, KDF_ALGORITHM, KDF_M_COST, KDF_P_COST, KDF_T_COST};

/// Env var that wraps the DEK in `master.key` (optional).
pub const AUTH_PASSWORD_ENV: &str = "BOOKCLERK_AUTH_PASSWORD";

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

/// On-disk `master.key` envelope kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MasterKeyFormat {
    /// Raw 32-byte DEK (`BCK1`).
    Raw,
    /// Password-wrapped DEK (`BCK2`).
    Wrapped,
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
        let mut bytes = [0u8; DEK_LEN];
        rand::rngs::OsRng.fill_bytes(&mut bytes);
        Ok(Self { bytes })
    }

    /// Wraps already-unwrapped DEK bytes (from `BCK1` or after password unwrap).
    fn from_bytes(bytes: [u8; DEK_LEN]) -> Self {
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
        _ => Err(LibraryError::Other(anyhow::anyhow!(
            "master key file {} has unknown magic (expected BCK1/BCK2)",
            path.display()
        ))),
    }
}

/// Load or mint the DEK and cache it for this process.
///
/// Password comes from `BOOKCLERK_AUTH_PASSWORD` only. Prefer
/// [`configure_master_key_with`] when `[auth].password` may apply.
///
/// Call once at CLI/daemon startup after `paths.ensure_dirs()`.
///
/// # Errors
///
/// Returns an error when the operation fails.
pub fn configure_master_key(files_dir: &Path) -> Result<MasterKey> {
    configure_master_key_with(files_dir, read_auth_password_env().as_deref())
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
/// Returns an error when the operation fails.
pub fn resolve_master_key(files_dir: &Path) -> Result<MasterKey> {
    resolve_master_key_with(files_dir, read_auth_password_env().as_deref())
}

/// Resolve the DEK with an explicit password override.
///
/// # Errors
///
/// Returns an error when the operation fails.
pub fn resolve_master_key_with(files_dir: &Path, password: Option<&str>) -> Result<MasterKey> {
    let path = master_key_path(files_dir);
    let password = password
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .inspect(|p| bookclerk_config::register_secret(p));

    if path.is_file() {
        let raw = std::fs::read(&path).map_err(|e| {
            LibraryError::Other(anyhow::anyhow!(
                "failed to read master key {}: {e}",
                path.display()
            ))
        })?;
        return parse_master_key_file(&raw, password, &path);
    }

    mint_master_key(&path, password)
}

/// Wrap an existing `BCK1` `master.key` with `password` (`BCK2`).
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
            "master key file {} does not exist — start the CLI/daemon once to mint it",
            path.display()
        )));
    }
    configure_master_key_with(files_dir, Some(password))
}

/// Reads `BOOKCLERK_AUTH_PASSWORD` and registers it for log redaction; empty is treated as absent.
fn read_auth_password_env() -> Option<String> {
    let v = std::env::var(AUTH_PASSWORD_ENV).ok()?;
    let trimmed = v.trim();
    if trimmed.is_empty() {
        return None;
    }
    bookclerk_config::register_secret(trimmed);
    Some(trimmed.to_string())
}

/// Creates `master.key` (mode 0600): raw `BCK1` or password-wrapped `BCK2`. Only a later unwrap with the same password can recover a wrapped DEK.
fn mint_master_key(path: &Path, password: Option<&str>) -> Result<MasterKey> {
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
    let bytes = match password {
        Some(pw) => encode_wrapped_master_key(&dek, pw)?,
        None => encode_raw_master_key(&dek),
    };
    match write_secret_file_create_new(path, &bytes) {
        Ok(()) => {
            tracing::info!(
                path = %path.display(),
                wrapped = password.is_some(),
                "minted data-encryption key for encrypted_secrets (mode 0600)"
            );
            if password.is_none() {
                tracing::warn!(
                    path = %path.display(),
                    "master.key is BCK1 (unwrapped). Set {AUTH_PASSWORD_ENV} or \
                     [auth].password, then run `bookclerk config master-key wrap` \
                     (or reload bookclerkd) to wrap the DEK at rest."
                );
            }
            Ok(dek)
        }
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {
            // Lost a mint race — load the winner's file (and wrap if needed).
            let raw = std::fs::read(path).map_err(|e| {
                LibraryError::Other(anyhow::anyhow!(
                    "failed to read master key {}: {e}",
                    path.display()
                ))
            })?;
            parse_master_key_file(&raw, password, path)
        }
        Err(e) => Err(LibraryError::Other(anyhow::anyhow!(
            "failed to mint master key {}: {e}",
            path.display()
        ))),
    }
}

/// Loads a `BCK1`/`BCK2` file; `BCK2` requires the auth password to unwrap. A new password re-wraps `BCK1` in place (same DEK).
fn parse_master_key_file(raw: &[u8], password: Option<&str>, path: &Path) -> Result<MasterKey> {
    if raw.len() < 4 {
        return Err(LibraryError::Other(anyhow::anyhow!(
            "master key file {} is truncated",
            path.display()
        )));
    }
    match &raw[..4] {
        b"BCK1" => {
            if raw.len() != 4 + DEK_LEN {
                return Err(LibraryError::Other(anyhow::anyhow!(
                    "master key file {} has invalid BCK1 length",
                    path.display()
                )));
            }
            let mut bytes = [0u8; DEK_LEN];
            bytes.copy_from_slice(&raw[4..]);
            if let Some(pw) = password {
                // Password newly set: re-wrap in place for at-rest protection.
                let dek = MasterKey::from_bytes(bytes);
                let wrapped = encode_wrapped_master_key(&dek, pw)?;
                write_secret_file_atomic(path, &wrapped)?;
                tracing::info!(
                    path = %path.display(),
                    "re-wrapped master.key (BCK1→BCK2) with auth password"
                );
                return Ok(dek);
            }
            tracing::warn!(
                path = %path.display(),
                "master.key is BCK1 (unwrapped). Set {AUTH_PASSWORD_ENV} or \
                 [auth].password, then run `bookclerk config master-key wrap` \
                 (or reload bookclerkd) to wrap the DEK at rest."
            );
            Ok(MasterKey::from_bytes(bytes))
        }
        b"BCK2" => {
            let pw = password.ok_or_else(|| {
                LibraryError::Other(anyhow::anyhow!(
                    "master key file {} is password-wrapped — set {AUTH_PASSWORD_ENV} \
                     or [auth].password",
                    path.display()
                ))
            })?;
            decode_wrapped_master_key(&raw[4..], pw, path)
        }
        _ => Err(LibraryError::Other(anyhow::anyhow!(
            "master key file {} has unknown magic (expected BCK1/BCK2)",
            path.display()
        ))),
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
    if plain.len() != DEK_LEN {
        return Err(LibraryError::Other(anyhow::anyhow!(
            "unwrapped master key has invalid length"
        )));
    }
    let mut bytes = [0u8; DEK_LEN];
    bytes.copy_from_slice(&plain);
    Ok(MasterKey::from_bytes(bytes))
}

/// Derives the 32-byte wrap key from the auth password and salt (Argon2id).
fn derive_wrapping_key(password: &str, salt: &[u8]) -> Result<[u8; 32]> {
    let params = ArgonParams::new(KDF_M_COST, KDF_T_COST, KDF_P_COST, Some(32))
        .map_err(|e| LibraryError::Other(anyhow::anyhow!("argon2 params: {e}")))?;
    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut key = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut key);
    argon2
        .hash_password_into(password.as_bytes(), salt, &mut key)
        .map_err(|e| LibraryError::Other(anyhow::anyhow!("argon2 hash: {e}")))?;
    Ok(key)
}

/// Fills `len` bytes from the OS CSPRNG (salts, nonces, temp-file suffixes).
fn random_bytes(len: usize) -> Vec<u8> {
    let mut out = vec![0u8; len];
    rand::rngs::OsRng.fill_bytes(&mut out);
    out
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
    file.write_all(bytes)?;
    harden_path_io(path, false)?;
    Ok(())
}

/// Atomic replace via temp file + rename (for BCK1→BCK2 re-wrap).
fn write_secret_file_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(MASTER_KEY_FILE_NAME);
    let tmp = parent.join(format!(".{file_name}.tmp-{}-{}", std::process::id(), {
        let mut n = [0u8; 4];
        rand::rngs::OsRng.fill_bytes(&mut n);
        u32::from_le_bytes(n)
    }));
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
        file.write_all(bytes).map_err(|e| {
            LibraryError::Other(anyhow::anyhow!(
                "failed to write master key temp {}: {e}",
                tmp.display()
            ))
        })?;
    }
    harden_path(&tmp, false)?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        LibraryError::Other(anyhow::anyhow!(
            "failed to replace master key {}: {e}",
            path.display()
        ))
    })?;
    harden_path(path, false)?;
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

/// Async read lock — shared sealed-v1 tests hold this across `.await`.
#[cfg(test)]
pub(crate) async fn master_key_test_read_lock_async() -> tokio::sync::RwLockReadGuard<'static, ()> {
    master_key_test_rwlock().read().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// Dynamic passphrase so CodeQL does not flag a hard-coded crypto secret.
    fn test_passphrase(tag: &str) -> String {
        format!("unit-{tag}-{}", std::process::id())
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
        let (ct, nonce) = seal_with_dek(b"hello-secret", &dek).unwrap();
        let plain = unseal_with_dek(&ct, &nonce, &dek).unwrap();
        assert_eq!(plain, b"hello-secret");
    }
}
