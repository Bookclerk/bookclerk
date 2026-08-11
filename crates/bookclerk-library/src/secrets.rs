//! Encrypted secrets store — DB-backed credential storage.
//!
//! # Overview
//!
//! Secrets are kept in the `encrypted_secrets` table (M10 migration). Each row
//! stores opaque `ciphertext` with column-level metadata describing which KDF
//! and cipher were used, so every secret carries its own self-contained
//! decryption recipe.
//!
//! ## Algorithms
//!
//! - **New writes**: `sealed-v1` — XChaCha20-Poly1305 with the process DEK
//!   (from `master.key`). No per-row Argon2. No plaintext writes ever.
//! - **Legacy reads**: `json-encrypted` (Argon2id + BOOKCLERK_AUTH_PASSWORD)
//!   still decrypted. `json` (plaintext) is migrated on read if master key
//!   is available, otherwise rejected.
//!
//! ## Unseal cache
//!
//! [`unseal_secret`] keeps a process-wide plaintext cache (identity +
//! ciphertext fingerprint). Content-source plugins should just call
//! load/unseal normally — repeated acquire/scan loads stay cheap without
//! per-plugin caches. Upsert/delete invalidate the matching entries.
//!
//! ## Bootstrap secrets (NOT stored here)
//!
//! `BOOKCLERK_AUTH_PASSWORD`, `BOOKCLERK_DATABASE_POSTGRES_URL`, and
//! `BOOKCLERK_D1_API_TOKEN` / `CLOUDFLARE_API_TOKEN` are **env-only bootstrap**
//! credentials. They are needed to open the DB or bootstrap the master key and
//! cannot be stored here. `config.toml` also stays on disk.
//!
//! `BOOKCLERK_OPERATOR_TOKEN` is an optional **env override** for the durable
//! operator token row (`kind = operator_token`); see [`crate::operator_token`].

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use argon2::{Algorithm, Argon2, Params as ArgonParams, Version};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use chacha20poly1305::{
    aead::{Aead, KeyInit},
    XChaCha20Poly1305, XNonce,
};
use chrono::Utc;
use rand::RngCore;
use sea_orm::sea_query::OnConflict;
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, Set};
use serde::{Deserialize, Serialize};

use crate::entities::encrypted_secrets;
use crate::error::{LibraryError, Result};
use crate::master_key::{require_master_key, seal_with_dek, unseal_with_dek};

// ── Secret kinds ────────────────────────────────────────────────────────────

/// Well-known `kind` values for the `encrypted_secrets` table.
///
/// Runtime credentials live here. Bootstrap credentials
/// (`BOOKCLERK_AUTH_PASSWORD`, `BOOKCLERK_D1_API_TOKEN`,
/// `BOOKCLERK_DATABASE_POSTGRES_URL`) are env-only and never stored in this
/// table. `BOOKCLERK_OPERATOR_TOKEN` may override the durable
/// [`OPERATOR_TOKEN`](secret_kind::OPERATOR_TOKEN) row without writing it.
pub mod secret_kind {
    /// Store / source OAuth credentials (Audible, Libro.fm, Chirp, GA).
    pub const SOURCE_AUTH: &str = "source_auth";
    /// S3 / object-storage credentials.
    pub const S3: &str = "s3";
    /// Widevine L3 CDM device blob (`kind = widevine`).
    pub const WIDEVINE: &str = "widevine";
    /// Daemon HTTP operator API token (Bearer / GUI login).
    pub const OPERATOR_TOKEN: &str = "operator_token";
}

/// Ownership namespace for `encrypted_secrets.account_type`.
///
/// Rows tagged `integration` are store or portal credentials tied to a user
/// account; they are purged when that account is revoked. Rows tagged
/// `operator` are destination / control-plane secrets (e.g. S3 keys) that
/// outlive any individual store account and must never be touched by
/// [`delete_secrets_for_account`].
pub mod secret_account_type {
    /// Store / portal integration accounts (Audible, Libro, Chirp, GA, Widevine).
    pub const INTEGRATION: &str = "integration";
    /// Operator-owned destination / control-plane secrets (S3, …).
    pub const OPERATOR: &str = "operator";
}

// ── Format constants ─────────────────────────────────────────────────────────

/// Format tag for new writes: DEK-sealed XChaCha20-Poly1305 (no per-row Argon2).
pub const FORMAT_SEALED_V1: &str = "sealed-v1";

// ── Process-wide unseal cache ────────────────────────────────────────────────
//
// Plugins call `get` + `unseal_secret` on every title; Argon2 (legacy) and even
// XChaCha unseal are wasted work when the ciphertext has not changed. Cache
// plaintext here — keyed by secret identity + ciphertext fingerprint — so
// Libro/Chirp/GA/Audible stay cache-unaware. Upsert/delete invalidate.

#[derive(Clone, Eq, PartialEq, Hash)]
struct SecretCacheKey {
    kind: String,
    provider: String,
    account_type: String,
    account_id: String,
    name: String,
}

struct CachedPlaintext {
    /// Fingerprint of ciphertext (+ nonce) so a stale identity cannot hit.
    fingerprint: u64,
    plaintext: Vec<u8>,
}

fn plaintext_cache() -> &'static Mutex<HashMap<SecretCacheKey, CachedPlaintext>> {
    static CACHE: OnceLock<Mutex<HashMap<SecretCacheKey, CachedPlaintext>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cache_key_for(record: &EncryptedSecretRecord) -> SecretCacheKey {
    SecretCacheKey {
        kind: record.kind.clone(),
        provider: record.provider.clone().unwrap_or_default(),
        account_type: record.account_type.clone(),
        account_id: record.account_id.clone().unwrap_or_default(),
        name: record.name.clone(),
    }
}

fn cache_key_parts(
    kind: &str,
    provider: Option<&str>,
    account_type: &str,
    account_id: Option<&str>,
    name: &str,
) -> SecretCacheKey {
    SecretCacheKey {
        kind: kind.to_string(),
        provider: provider.unwrap_or("").to_string(),
        account_type: account_type.to_string(),
        account_id: account_id.unwrap_or("").to_string(),
        name: name.to_string(),
    }
}

fn ciphertext_fingerprint(record: &EncryptedSecretRecord) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    record.format.hash(&mut h);
    record.ciphertext.hash(&mut h);
    record.cipher_nonce.hash(&mut h);
    record.kdf_salt.hash(&mut h);
    h.finish()
}

fn cache_get(record: &EncryptedSecretRecord) -> Option<Vec<u8>> {
    let key = cache_key_for(record);
    let fp = ciphertext_fingerprint(record);
    let Ok(guard) = plaintext_cache().lock() else {
        return None;
    };
    guard.get(&key).and_then(|entry| {
        if entry.fingerprint == fp {
            Some(entry.plaintext.clone())
        } else {
            None
        }
    })
}

fn cache_put(record: &EncryptedSecretRecord, plaintext: &[u8]) {
    let Ok(mut guard) = plaintext_cache().lock() else {
        return;
    };
    guard.insert(
        cache_key_for(record),
        CachedPlaintext {
            fingerprint: ciphertext_fingerprint(record),
            plaintext: plaintext.to_vec(),
        },
    );
}

fn cache_invalidate_key(key: &SecretCacheKey) {
    if let Ok(mut guard) = plaintext_cache().lock() {
        guard.remove(key);
    }
}

fn cache_invalidate_account(account_id: &str) {
    if let Ok(mut guard) = plaintext_cache().lock() {
        guard.retain(|k, _| k.account_id != account_id);
    }
}

/// Drop every cached plaintext unseal.
///
/// Call when the process DEK identity changes. Password wrap of the same DEK
/// (`BCK1` → `BCK2`) does not require this — ciphertext and plaintext stay valid.
pub fn clear_unseal_cache() {
    if let Ok(mut guard) = plaintext_cache().lock() {
        guard.clear();
    }
}

#[cfg(test)]
fn clear_plaintext_cache_for_tests() {
    clear_unseal_cache();
}

// ── Record ───────────────────────────────────────────────────────────────────

/// A row from the `encrypted_secrets` table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncryptedSecretRecord {
    /// Surrogate primary key assigned by the database.
    pub id: Option<i64>,
    /// Secret kind discriminator (`source_auth`, `s3`, …).
    pub kind: String,
    /// Source / service name (`"audible"`, `"libro"`, `"s3"`, …) or `None`.
    pub provider: Option<String>,
    /// Ownership namespace — see [`secret_account_type`] constants.
    pub account_type: String,
    /// Per-provider account identifier (file stem, email, …) or `None`.
    pub account_id: Option<String>,
    /// Human-readable label / file-stem equivalent (e.g. `"alice.audible"`).
    pub name: String,
    /// Payload format:
    /// - `"sealed-v1"` — XChaCha20-Poly1305 with process DEK (no per-row Argon2)
    /// - `"audible-rs-auth"` — sealed-v1 wrapping a plain audible-rs envelope
    /// - `"json-encrypted"` — legacy: JSON encrypted with Argon2id + XChaCha20-Poly1305
    pub format: String,
    /// Encrypted payload bytes for this secret.
    pub ciphertext: Vec<u8>,
    /// KDF algorithm id for legacy rows (for example `argon2id`).
    pub kdf_algorithm: Option<String>,
    /// Random salt for legacy Argon2 key derivation.
    pub kdf_salt: Option<Vec<u8>>,
    /// Argon2 memory cost in KiB for legacy rows.
    pub kdf_m_cost: Option<u32>,
    /// Argon2 time cost (iterations) for legacy rows.
    pub kdf_t_cost: Option<u32>,
    /// Argon2 parallel lane count stored for legacy `json-encrypted` rows.
    pub kdf_p_cost: Option<u32>,
    /// Cipher algorithm identifier (e.g. `"xchacha20poly1305"`) or `None`.
    pub cipher_algorithm: Option<String>,
    /// AEAD nonce bytes used with `ciphertext`.
    pub cipher_nonce: Option<Vec<u8>>,
    /// RFC 3339 timestamp when the row was inserted.
    pub created_at: String,
    /// RFC 3339 timestamp when the row was last modified.
    pub updated_at: String,
}

// ── Encryption constants ─────────────────────────────────────────────────────

/// Argon2id memory cost in KiB (64 MiB — OWASP minimum). Legacy only.
pub const KDF_M_COST: u32 = 65_536;
/// Argon2id time cost (iterations). Legacy only.
pub const KDF_T_COST: u32 = 3;
/// Argon2id parallelism factor. Legacy only.
pub const KDF_P_COST: u32 = 1;
/// KDF algorithm identifier. Legacy only.
pub const KDF_ALGORITHM: &str = "argon2id";
/// Cipher algorithm identifier stored alongside ciphertext rows.
pub const CIPHER_ALGORITHM: &str = "xchacha20poly1305";
const SALT_LEN: usize = 16;
/// XChaCha20 uses a 192-bit (24-byte) nonce.
const NONCE_LEN: usize = 24;

// ── Legacy encryption helpers (json-encrypted read / migration) ───────────────

/// Raw output from [`encrypt_secret`] (legacy Argon2id path).
pub struct EncryptedBlob {
    /// Random salt for legacy Argon2 key derivation.
    pub kdf_salt: Vec<u8>,
    /// AEAD nonce bytes used with `ciphertext`.
    pub cipher_nonce: Vec<u8>,
    /// Encrypted payload bytes for this secret.
    pub ciphertext: Vec<u8>,
}

/// Derive a 32-byte key from `password` + `salt` using Argon2id (legacy).
fn derive_key(password: &str, salt: &[u8]) -> Result<[u8; 32]> {
    let params = ArgonParams::new(KDF_M_COST, KDF_T_COST, KDF_P_COST, Some(32))
        .map_err(|e| LibraryError::Other(anyhow::anyhow!("argon2 params: {e}")))?;
    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut key = random_bytes_array::<32>();
    argon2
        .hash_password_into(password.as_bytes(), salt, &mut key)
        .map_err(|e| LibraryError::Other(anyhow::anyhow!("argon2 hash: {e}")))?;
    Ok(key)
}

fn random_bytes_array<const N: usize>() -> [u8; N] {
    let mut out = vec![0_u8; N];
    rand::rngs::OsRng.fill_bytes(&mut out);
    out.try_into().expect("random buffer length matches N")
}

/// Encrypt `plaintext` with Argon2id key derivation + XChaCha20-Poly1305 (legacy).
///
/// Use [`build_sealed_record`] for new writes. This function is retained for
/// legacy test compat and the json-encrypted migration path.
pub fn encrypt_secret(plaintext: &[u8], password: &str) -> Result<EncryptedBlob> {
    let salt = random_bytes_array::<SALT_LEN>().to_vec();
    let nonce_bytes = random_bytes_array::<NONCE_LEN>().to_vec();

    let key = derive_key(password, &salt)?;
    let cipher = XChaCha20Poly1305::new_from_slice(&key)
        .map_err(|e| LibraryError::Other(anyhow::anyhow!("cipher init: {e}")))?;
    let nonce = XNonce::try_from(nonce_bytes.as_slice())
        .map_err(|_| LibraryError::Other(anyhow::anyhow!("invalid encryption nonce length")))?;
    let ciphertext = cipher
        .encrypt(&nonce, plaintext)
        .map_err(|_| LibraryError::Other(anyhow::anyhow!("xchacha20poly1305 encryption failed")))?;

    Ok(EncryptedBlob {
        kdf_salt: salt,
        cipher_nonce: nonce_bytes,
        ciphertext,
    })
}

/// Decrypt ciphertext using Argon2id + XChaCha20-Poly1305 (legacy `json-encrypted`).
///
/// Use [`unseal_secret`] for new reads. This function handles legacy DB rows.
pub fn decrypt_secret(
    ciphertext: &[u8],
    password: &str,
    kdf_salt: &[u8],
    cipher_nonce: &[u8],
) -> Result<Vec<u8>> {
    let key = derive_key(password, kdf_salt)?;
    let cipher = XChaCha20Poly1305::new_from_slice(&key)
        .map_err(|e| LibraryError::Other(anyhow::anyhow!("cipher init: {e}")))?;
    let nonce = XNonce::try_from(cipher_nonce)
        .map_err(|_| LibraryError::Other(anyhow::anyhow!("invalid decryption nonce length")))?;
    let plaintext = cipher.decrypt(&nonce, ciphertext).map_err(|_| {
        LibraryError::Other(anyhow::anyhow!(
            "decryption failed — wrong password or corrupted ciphertext"
        ))
    })?;
    Ok(plaintext)
}

// ── Master-key seal / unseal helpers ─────────────────────────────────────────

/// Build a [`EncryptedSecretRecord`] skeleton sealed with the process DEK.
///
/// Callers must fill in `kind`, `provider`, `account_type`, `account_id`,
/// `name`, and timestamps before upserting.
pub fn build_sealed_record(
    plaintext: &[u8],
    kind: &str,
    provider: &str,
    account_type: &str,
    account_id: &str,
    name: &str,
) -> Result<EncryptedSecretRecord> {
    let dek = require_master_key(None)?;
    let (ciphertext, nonce) = seal_with_dek(plaintext, &dek)?;
    let now = now_str();
    Ok(EncryptedSecretRecord {
        id: None,
        kind: kind.to_string(),
        provider: Some(provider.to_string()),
        account_type: account_type.to_string(),
        account_id: Some(account_id.to_string()),
        name: name.to_string(),
        format: FORMAT_SEALED_V1.to_string(),
        ciphertext,
        kdf_algorithm: None,
        kdf_salt: None,
        kdf_m_cost: None,
        kdf_t_cost: None,
        kdf_p_cost: None,
        cipher_algorithm: Some(CIPHER_ALGORITHM.to_string()),
        cipher_nonce: Some(nonce),
        created_at: now.clone(),
        updated_at: now,
    })
}

/// Unseal a `sealed-v1` record using the process DEK.
///
/// Also handles legacy `json-encrypted` (using `BOOKCLERK_AUTH_PASSWORD`) and
/// rejects `json` plaintext.
///
/// Successful unseals are cached process-wide (keyed by secret identity +
/// ciphertext fingerprint) so repeated loads during acquire/scan do not
/// re-decrypt. Callers — including content-source plugins — need not cache.
pub fn unseal_secret(record: &EncryptedSecretRecord) -> Result<Vec<u8>> {
    if let Some(cached) = cache_get(record) {
        return Ok(cached);
    }
    let plain = unseal_secret_uncached(record)?;
    cache_put(record, &plain);
    Ok(plain)
}

fn unseal_secret_uncached(record: &EncryptedSecretRecord) -> Result<Vec<u8>> {
    match record.format.as_str() {
        FORMAT_SEALED_V1 => {
            let nonce = record.cipher_nonce.as_deref().ok_or_else(|| {
                LibraryError::Other(anyhow::anyhow!(
                    "sealed-v1 record {} missing cipher_nonce",
                    record.name
                ))
            })?;
            let dek = require_master_key(None)?;
            unseal_with_dek(&record.ciphertext, nonce, &dek)
        }
        "json-encrypted" => {
            let password = read_auth_password_for_legacy(&record.name)?;
            let salt = record.kdf_salt.as_deref().ok_or_else(|| {
                LibraryError::Other(anyhow::anyhow!(
                    "json-encrypted record {} missing kdf_salt",
                    record.name
                ))
            })?;
            let nonce = record.cipher_nonce.as_deref().ok_or_else(|| {
                LibraryError::Other(anyhow::anyhow!(
                    "json-encrypted record {} missing cipher_nonce",
                    record.name
                ))
            })?;
            decrypt_secret(&record.ciphertext, &password, salt, nonce)
        }
        "json" => Err(LibraryError::Other(anyhow::anyhow!(
            "plaintext secrets are no longer supported — record {} must be migrated to sealed-v1",
            record.name
        ))),
        other => Err(LibraryError::Other(anyhow::anyhow!(
            "unknown secret format {other:?} for record {}",
            record.name
        ))),
    }
}

fn read_auth_password_for_legacy(name: &str) -> Result<String> {
    let v = std::env::var(crate::master_key::AUTH_PASSWORD_ENV).map_err(|_| {
        LibraryError::Other(anyhow::anyhow!(
            "legacy json-encrypted record {name} requires {env} — set it to migrate",
            env = crate::master_key::AUTH_PASSWORD_ENV
        ))
    })?;
    let trimmed = v.trim();
    if trimmed.is_empty() {
        return Err(LibraryError::Other(anyhow::anyhow!(
            "legacy json-encrypted record {name} requires {env} (was empty)",
            env = crate::master_key::AUTH_PASSWORD_ENV
        )));
    }
    bookclerk_config::register_secret(trimmed);
    Ok(trimmed.to_string())
}

/// Seal arbitrary bytes as base64 string — used by D1 transport layer.
#[must_use]
pub fn bytes_to_b64_string(bytes: &[u8]) -> String {
    format!("b64:{}", BASE64.encode(bytes))
}

/// Decode a `b64:`-prefixed string to bytes. Returns `None` if not prefixed.
#[must_use]
pub fn b64_string_to_bytes(s: &str) -> Option<Vec<u8>> {
    s.strip_prefix("b64:").and_then(|b| BASE64.decode(b).ok())
}

// ── SecretStore ───────────────────────────────────────────────────────────────

/// Thin wrapper around a [`DatabaseConnection`] for `encrypted_secrets` CRUD.
///
/// Construct with [`SecretStore::new`] or call the standalone `async fn`
/// helpers directly. All methods are `async`.
pub struct SecretStore<'a> {
    db: &'a DatabaseConnection,
}

impl<'a> SecretStore<'a> {
    /// Constructs a store handle over an existing database connection.
    ///
    /// # Arguments
    ///
    /// * `db` - SeaORM connection owned by the caller for the lifetime of this handle.
    ///
    /// # Returns
    ///
    /// A [`SecretStore`] that delegates to the standalone CRUD helpers.
    pub fn new(db: &'a DatabaseConnection) -> Self {
        Self { db }
    }

    /// Inserts or replaces a sealed secret row.
    ///
    /// # Arguments
    ///
    /// * `record` - Fully populated sealed secret identity and ciphertext.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError`] when the database write fails.
    pub async fn upsert(&self, record: &EncryptedSecretRecord) -> Result<()> {
        upsert_secret(self.db, record).await
    }

    /// Loads one sealed secret by identity columns, if present.
    ///
    /// # Arguments
    ///
    /// * `kind` - Secret kind (`source_auth`, `s3`, …).
    /// * `provider` - Optional provider / plugin id filter.
    /// * `account_type` - `integration` or `operator`.
    /// * `account_id` - Optional account id filter.
    /// * `name` - Logical secret name within that identity.
    ///
    /// # Returns
    ///
    /// `Ok(Some(record))` when found, `Ok(None)` otherwise.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError`] when the database read fails.
    pub async fn get(
        &self,
        kind: &str,
        provider: Option<&str>,
        account_type: &str,
        account_id: Option<&str>,
        name: &str,
    ) -> Result<Option<EncryptedSecretRecord>> {
        get_secret(self.db, kind, provider, account_type, account_id, name).await
    }

    /// Lists sealed secrets with the given `kind`.
    ///
    /// # Arguments
    ///
    /// * `kind` - Secret kind to filter on.
    ///
    /// # Returns
    ///
    /// All matching sealed rows (ciphertext included).
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError`] when the database read fails.
    pub async fn list(&self, kind: &str) -> Result<Vec<EncryptedSecretRecord>> {
        list_secrets(self.db, kind).await
    }

    /// Deletes one sealed secret by identity columns.
    ///
    /// # Arguments
    ///
    /// * `kind` - Secret kind (`source_auth`, `s3`, …).
    /// * `provider` - Optional provider / plugin id filter.
    /// * `account_type` - `integration` or `operator`.
    /// * `account_id` - Optional account id filter.
    /// * `name` - Logical secret name within that identity.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError`] when the database delete fails.
    pub async fn delete(
        &self,
        kind: &str,
        provider: Option<&str>,
        account_type: &str,
        account_id: Option<&str>,
        name: &str,
    ) -> Result<()> {
        delete_secret(self.db, kind, provider, account_type, account_id, name).await
    }
}

// ── Standalone CRUD functions (typed SeaORM) ──────────────────────────────────

/// Insert or replace a secret in the DB using a single ON CONFLICT statement.
///
/// provider and account_id should always be `Some` for new writes.
pub async fn upsert_secret(db: &DatabaseConnection, record: &EncryptedSecretRecord) -> Result<()> {
    let now = now_str();
    let am = encrypted_secrets::ActiveModel {
        id: sea_orm::NotSet,
        kind: Set(record.kind.clone()),
        provider: Set(record.provider.clone()),
        account_type: Set(record.account_type.clone()),
        account_id: Set(record.account_id.clone()),
        name: Set(record.name.clone()),
        format: Set(record.format.clone()),
        ciphertext: Set(record.ciphertext.clone()),
        kdf_algorithm: Set(record.kdf_algorithm.clone()),
        kdf_salt: Set(record.kdf_salt.clone()),
        kdf_m_cost: Set(record.kdf_m_cost.map(i64::from)),
        kdf_t_cost: Set(record.kdf_t_cost.map(i64::from)),
        kdf_p_cost: Set(record.kdf_p_cost.map(i64::from)),
        cipher_algorithm: Set(record.cipher_algorithm.clone()),
        cipher_nonce: Set(record.cipher_nonce.clone()),
        created_at: Set(if record.created_at.is_empty() {
            now.clone()
        } else {
            record.created_at.clone()
        }),
        updated_at: Set(now),
    };

    encrypted_secrets::Entity::insert(am)
        .on_conflict(
            OnConflict::columns([
                encrypted_secrets::Column::Kind,
                encrypted_secrets::Column::Provider,
                encrypted_secrets::Column::AccountType,
                encrypted_secrets::Column::AccountId,
                encrypted_secrets::Column::Name,
            ])
            .update_columns([
                encrypted_secrets::Column::Format,
                encrypted_secrets::Column::Ciphertext,
                encrypted_secrets::Column::KdfAlgorithm,
                encrypted_secrets::Column::KdfSalt,
                encrypted_secrets::Column::KdfMCost,
                encrypted_secrets::Column::KdfTCost,
                encrypted_secrets::Column::KdfPCost,
                encrypted_secrets::Column::CipherAlgorithm,
                encrypted_secrets::Column::CipherNonce,
                encrypted_secrets::Column::UpdatedAt,
            ])
            .to_owned(),
        )
        .exec(db)
        .await
        .map_err(LibraryError::Orm)?;
    cache_invalidate_key(&cache_key_parts(
        &record.kind,
        record.provider.as_deref(),
        &record.account_type,
        record.account_id.as_deref(),
        &record.name,
    ));
    Ok(())
}

/// Fetch a single secret by its composite key. Returns `None` if not found.
pub async fn get_secret(
    db: &DatabaseConnection,
    kind: &str,
    provider: Option<&str>,
    account_type: &str,
    account_id: Option<&str>,
    name: &str,
) -> Result<Option<EncryptedSecretRecord>> {
    Ok(
        find_model(db, kind, provider, account_type, account_id, name)
            .await?
            .map(model_to_record),
    )
}

/// List all secrets of a given `kind`.
pub async fn list_secrets(
    db: &DatabaseConnection,
    kind: &str,
) -> Result<Vec<EncryptedSecretRecord>> {
    let rows = encrypted_secrets::Entity::find()
        .filter(encrypted_secrets::Column::Kind.eq(kind))
        .all(db)
        .await
        .map_err(LibraryError::Orm)?;
    Ok(rows.into_iter().map(model_to_record).collect())
}

/// Delete integration-account secrets for `account_id`.
///
/// Operator-typed secrets (e.g. S3 destination credentials) are never
/// matched — they have `account_type = "operator"` and survive account
/// revocation.
pub async fn delete_secrets_for_account(db: &DatabaseConnection, account_id: &str) -> Result<()> {
    encrypted_secrets::Entity::delete_many()
        .filter(encrypted_secrets::Column::AccountType.eq(secret_account_type::INTEGRATION))
        .filter(encrypted_secrets::Column::AccountId.eq(account_id))
        .exec(db)
        .await
        .map_err(LibraryError::Orm)?;
    cache_invalidate_account(account_id);
    Ok(())
}

/// Delete a secret by its composite key. No-op if it does not exist.
pub async fn delete_secret(
    db: &DatabaseConnection,
    kind: &str,
    provider: Option<&str>,
    account_type: &str,
    account_id: Option<&str>,
    name: &str,
) -> Result<()> {
    let mut q = encrypted_secrets::Entity::delete_many()
        .filter(encrypted_secrets::Column::Kind.eq(kind))
        .filter(encrypted_secrets::Column::AccountType.eq(account_type))
        .filter(encrypted_secrets::Column::Name.eq(name));
    q = match provider {
        Some(p) => q.filter(encrypted_secrets::Column::Provider.eq(p)),
        None => q.filter(encrypted_secrets::Column::Provider.is_null()),
    };
    q = match account_id {
        Some(a) => q.filter(encrypted_secrets::Column::AccountId.eq(a)),
        None => q.filter(encrypted_secrets::Column::AccountId.is_null()),
    };
    q.exec(db).await.map_err(LibraryError::Orm)?;
    cache_invalidate_key(&cache_key_parts(
        kind,
        provider,
        account_type,
        account_id,
        name,
    ));
    Ok(())
}

// ── Internal helpers ──────────────────────────────────────────────────────────

fn now_str() -> String {
    Utc::now().to_rfc3339()
}

async fn find_model(
    db: &DatabaseConnection,
    kind: &str,
    provider: Option<&str>,
    account_type: &str,
    account_id: Option<&str>,
    name: &str,
) -> Result<Option<encrypted_secrets::Model>> {
    let mut q = encrypted_secrets::Entity::find()
        .filter(encrypted_secrets::Column::Kind.eq(kind))
        .filter(encrypted_secrets::Column::AccountType.eq(account_type))
        .filter(encrypted_secrets::Column::Name.eq(name));
    q = match provider {
        Some(p) => q.filter(encrypted_secrets::Column::Provider.eq(p)),
        None => q.filter(encrypted_secrets::Column::Provider.is_null()),
    };
    q = match account_id {
        Some(a) => q.filter(encrypted_secrets::Column::AccountId.eq(a)),
        None => q.filter(encrypted_secrets::Column::AccountId.is_null()),
    };
    q.one(db).await.map_err(LibraryError::Orm)
}

fn model_to_record(model: encrypted_secrets::Model) -> EncryptedSecretRecord {
    EncryptedSecretRecord {
        id: Some(model.id),
        kind: model.kind,
        provider: model.provider,
        account_type: model.account_type,
        account_id: model.account_id,
        name: model.name,
        format: model.format,
        ciphertext: model.ciphertext,
        kdf_algorithm: model.kdf_algorithm,
        kdf_salt: model.kdf_salt,
        kdf_m_cost: model.kdf_m_cost.map(|n| n as u32),
        kdf_t_cost: model.kdf_t_cost.map(|n| n as u32),
        kdf_p_cost: model.kdf_p_cost.map(|n| n as u32),
        cipher_algorithm: model.cipher_algorithm,
        cipher_nonce: model.cipher_nonce,
        created_at: model.created_at,
        updated_at: model.updated_at,
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::master_key::{
        configure_master_key, ensure_shared_test_dek, master_key_test_lock,
        master_key_test_read_lock_async,
    };
    use tempfile::tempdir;

    fn test_passphrase(tag: &str) -> String {
        format!("unit-{tag}-{}", std::process::id())
    }

    /// Shared process DEK for sealed-v1 tests (read-locked so mutators wait).
    async fn setup_dek() -> tokio::sync::RwLockReadGuard<'static, ()> {
        let guard = master_key_test_read_lock_async().await;
        ensure_shared_test_dek();
        guard
    }

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let plaintext = b"super secret audible token payload";
        let password = test_passphrase("argon2id");
        let blob = encrypt_secret(plaintext, &password).unwrap();
        let recovered = decrypt_secret(
            &blob.ciphertext,
            &password,
            &blob.kdf_salt,
            &blob.cipher_nonce,
        )
        .unwrap();
        assert_eq!(recovered, plaintext);
    }

    #[test]
    fn wrong_password_fails() {
        let good = test_passphrase("correct");
        let bad = test_passphrase("wrong");
        let blob = encrypt_secret(b"secret", &good).unwrap();
        let result = decrypt_secret(&blob.ciphertext, &bad, &blob.kdf_salt, &blob.cipher_nonce);
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn upsert_sealed_v1_and_get() {
        let _dek = setup_dek().await;
        let db = bookclerk_plugin_database_sqlite::open_memory()
            .await
            .unwrap();
        let plaintext = b"test-sealed-payload";
        let record = build_sealed_record(
            plaintext,
            secret_kind::SOURCE_AUTH,
            "libro",
            secret_account_type::INTEGRATION,
            "alice",
            "alice.libro.auth",
        )
        .unwrap();
        upsert_secret(&db, &record).await.unwrap();

        let fetched = get_secret(
            &db,
            secret_kind::SOURCE_AUTH,
            Some("libro"),
            secret_account_type::INTEGRATION,
            Some("alice"),
            "alice.libro.auth",
        )
        .await
        .unwrap()
        .unwrap();

        assert_eq!(fetched.format, FORMAT_SEALED_V1);
        let recovered = unseal_secret(&fetched).unwrap();
        assert_eq!(recovered, plaintext);
        // Second unseal must hit the process cache (same ciphertext).
        let again = unseal_secret(&fetched).unwrap();
        assert_eq!(again, plaintext);
    }

    #[test]
    fn dek_identity_change_clears_unseal_cache() {
        let _dek = master_key_test_lock();
        clear_plaintext_cache_for_tests();
        let dir1 = tempdir().unwrap();
        let dir2 = tempdir().unwrap();
        configure_master_key(dir1.path()).unwrap();
        let record = build_sealed_record(
            b"cached-plain",
            secret_kind::SOURCE_AUTH,
            "libro",
            secret_account_type::INTEGRATION,
            "c",
            "c.libro.auth",
        )
        .unwrap();
        assert_eq!(unseal_secret(&record).unwrap(), b"cached-plain");
        // Install a different DEK — wrap of the same key would NOT clear; a new
        // master.key must flush so we do not return the old plaintext.
        configure_master_key(dir2.path()).unwrap();
        assert!(
            unseal_secret(&record).is_err(),
            "stale plaintext must not be served after DEK identity change"
        );
        clear_plaintext_cache_for_tests();
    }

    #[tokio::test]
    async fn unseal_cache_invalidates_on_upsert() {
        let _dek = setup_dek().await;
        clear_plaintext_cache_for_tests();
        let db = bookclerk_plugin_database_sqlite::open_memory()
            .await
            .unwrap();
        let first = build_sealed_record(
            b"v1",
            secret_kind::SOURCE_AUTH,
            "libro",
            secret_account_type::INTEGRATION,
            "bob",
            "bob.libro.auth",
        )
        .unwrap();
        upsert_secret(&db, &first).await.unwrap();
        let fetched = get_secret(
            &db,
            secret_kind::SOURCE_AUTH,
            Some("libro"),
            secret_account_type::INTEGRATION,
            Some("bob"),
            "bob.libro.auth",
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(unseal_secret(&fetched).unwrap(), b"v1");

        let second = build_sealed_record(
            b"v2",
            secret_kind::SOURCE_AUTH,
            "libro",
            secret_account_type::INTEGRATION,
            "bob",
            "bob.libro.auth",
        )
        .unwrap();
        upsert_secret(&db, &second).await.unwrap();
        let fetched2 = get_secret(
            &db,
            secret_kind::SOURCE_AUTH,
            Some("libro"),
            secret_account_type::INTEGRATION,
            Some("bob"),
            "bob.libro.auth",
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(unseal_secret(&fetched2).unwrap(), b"v2");
    }

    #[tokio::test]
    async fn upsert_replaces_same_composite_key() {
        let _dek = setup_dek().await;
        let db = bookclerk_plugin_database_sqlite::open_memory()
            .await
            .unwrap();
        for i in 0u8..2 {
            let rec = build_sealed_record(
                &[i],
                secret_kind::S3,
                "s3",
                secret_account_type::OPERATOR,
                "default",
                "default",
            )
            .unwrap();
            upsert_secret(&db, &rec).await.unwrap();
        }
        let all = list_secrets(&db, secret_kind::S3).await.unwrap();
        assert_eq!(all.len(), 1);
        let recovered = unseal_secret(&all[0]).unwrap();
        assert_eq!(recovered, &[1u8]);
    }

    #[tokio::test]
    async fn list_secrets_by_kind() {
        let _dek = setup_dek().await;
        let db = bookclerk_plugin_database_sqlite::open_memory()
            .await
            .unwrap();
        for (provider, account_id, name) in &[
            ("libro", "alice", "alice.libro.auth"),
            ("chirp", "bob", "bob.chirp.auth"),
        ] {
            let rec = build_sealed_record(
                b"x",
                secret_kind::SOURCE_AUTH,
                provider,
                secret_account_type::INTEGRATION,
                account_id,
                name,
            )
            .unwrap();
            upsert_secret(&db, &rec).await.unwrap();
        }
        let secrets = list_secrets(&db, secret_kind::SOURCE_AUTH).await.unwrap();
        assert_eq!(secrets.len(), 2);
    }

    #[tokio::test]
    async fn delete_secret_test() {
        let _dek = setup_dek().await;
        let db = bookclerk_plugin_database_sqlite::open_memory()
            .await
            .unwrap();
        let rec = build_sealed_record(
            b"{}",
            secret_kind::S3,
            "s3",
            secret_account_type::OPERATOR,
            "default",
            "default",
        )
        .unwrap();
        upsert_secret(&db, &rec).await.unwrap();

        delete_secret(
            &db,
            secret_kind::S3,
            Some("s3"),
            secret_account_type::OPERATOR,
            Some("default"),
            "default",
        )
        .await
        .unwrap();

        let result = get_secret(
            &db,
            secret_kind::S3,
            Some("s3"),
            secret_account_type::OPERATOR,
            Some("default"),
            "default",
        )
        .await
        .unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn delete_secrets_for_account_only_integration() {
        let _dek = setup_dek().await;
        let db = bookclerk_plugin_database_sqlite::open_memory()
            .await
            .unwrap();

        let auth_rec = build_sealed_record(
            b"auth",
            secret_kind::SOURCE_AUTH,
            "audible",
            secret_account_type::INTEGRATION,
            "alice",
            "alice.audible.auth",
        )
        .unwrap();
        upsert_secret(&db, &auth_rec).await.unwrap();

        // S3 row: account_type=operator, account_id="default" — must never be touched.
        let s3_rec = build_sealed_record(
            b"s3creds",
            secret_kind::S3,
            "s3",
            secret_account_type::OPERATOR,
            "default",
            "default",
        )
        .unwrap();
        upsert_secret(&db, &s3_rec).await.unwrap();

        // Deleting integration secrets for "alice" must not touch the operator S3 row.
        delete_secrets_for_account(&db, "alice").await.unwrap();

        let auth = get_secret(
            &db,
            secret_kind::SOURCE_AUTH,
            Some("audible"),
            secret_account_type::INTEGRATION,
            Some("alice"),
            "alice.audible.auth",
        )
        .await
        .unwrap();
        assert!(auth.is_none(), "source_auth for alice should be deleted");

        let s3 = get_secret(
            &db,
            secret_kind::S3,
            Some("s3"),
            secret_account_type::OPERATOR,
            Some("default"),
            "default",
        )
        .await
        .unwrap();
        assert!(s3.is_some(), "operator S3 row must survive");

        // Also: deleting integration "default" must not delete the operator S3 row.
        delete_secrets_for_account(&db, "default").await.unwrap();
        let s3_still = get_secret(
            &db,
            secret_kind::S3,
            Some("s3"),
            secret_account_type::OPERATOR,
            Some("default"),
            "default",
        )
        .await
        .unwrap();
        assert!(
            s3_still.is_some(),
            "operator S3 row must survive even when deleting account_id='default' as integration"
        );
    }

    #[tokio::test]
    async fn b64_roundtrip() {
        let bytes = vec![0u8, 1, 2, 200, 255];
        let encoded = bytes_to_b64_string(&bytes);
        assert!(encoded.starts_with("b64:"));
        let decoded = b64_string_to_bytes(&encoded).unwrap();
        assert_eq!(decoded, bytes);
    }

    #[test]
    fn unseal_rejects_json_plaintext() {
        let rec = EncryptedSecretRecord {
            id: None,
            kind: "source_auth".into(),
            provider: Some("libro".into()),
            account_type: secret_account_type::INTEGRATION.into(),
            account_id: Some("alice".into()),
            name: "alice.libro.auth".into(),
            format: "json".into(),
            ciphertext: br#"{"token":"test"}"#.to_vec(),
            kdf_algorithm: None,
            kdf_salt: None,
            kdf_m_cost: None,
            kdf_t_cost: None,
            kdf_p_cost: None,
            cipher_algorithm: None,
            cipher_nonce: None,
            created_at: "2024-01-01T00:00:00Z".into(),
            updated_at: "2024-01-01T00:00:00Z".into(),
        };
        let err = unseal_secret(&rec).unwrap_err();
        assert!(err.to_string().contains("plaintext") || err.to_string().contains("sealed-v1"));
    }
}
