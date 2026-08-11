//! SeaORM entity for the `encrypted_secrets` table.
//!
//! Retained in the schema and entities per the multi-storefront design; the
//! encryption / secrets-file removal work lives in [`crate::secrets`].

use sea_orm::entity::prelude::*;

/// Row shape for this table (`DeriveEntityModel`).
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "encrypted_secrets")]
pub struct Model {
    /// Surrogate primary key assigned by the database.
    #[sea_orm(primary_key)]
    pub id: i64,
    /// Secret kind discriminator (`source_auth`, `s3`, …).
    pub kind: String,
    /// External identity or integration provider id (for example ABS).
    pub provider: Option<String>,
    /// Ownership namespace: [`crate::secrets::secret_account_type`].
    pub account_type: String,
    /// Store or operator account id this row belongs to.
    pub account_id: Option<String>,
    /// Human-readable or logical name for this row.
    pub name: String,
    /// Sealing format tag (`sealed-v1`, legacy `json-encrypted`, …).
    pub format: String,
    /// Encrypted payload bytes for this secret.
    pub ciphertext: Vec<u8>,
    /// KDF algorithm id for legacy rows (for example `argon2id`).
    pub kdf_algorithm: Option<String>,
    /// Random salt for legacy Argon2 key derivation.
    pub kdf_salt: Option<Vec<u8>>,
    /// Argon2 memory cost in KiB for legacy rows.
    pub kdf_m_cost: Option<i64>,
    /// Argon2 time cost (iterations) for legacy rows.
    pub kdf_t_cost: Option<i64>,
    /// Argon2 parallel lane count stored for legacy `json-encrypted` rows.
    pub kdf_p_cost: Option<i64>,
    /// AEAD algorithm id (for example `xchacha20poly1305`).
    pub cipher_algorithm: Option<String>,
    /// AEAD nonce bytes used with `ciphertext`.
    pub cipher_nonce: Option<Vec<u8>>,
    /// RFC 3339 timestamp when the row was inserted.
    pub created_at: String,
    /// RFC 3339 timestamp when the row was last modified.
    pub updated_at: String,
}

/// Declared SeaORM relations (none unless FK edges are modeled).
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
