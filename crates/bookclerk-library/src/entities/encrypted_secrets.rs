//! `encrypted_secrets` table entity.
//!
//! Retained in the schema and entities per the multi-storefront design; the
//! encryption / secrets-file removal work lives in [`crate::secrets`].

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "encrypted_secrets")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub kind: String,
    pub provider: Option<String>,
    /// Ownership namespace: [`crate::secrets::secret_account_type`].
    pub account_type: String,
    pub account_id: Option<String>,
    pub name: String,
    pub format: String,
    pub ciphertext: Vec<u8>,
    pub kdf_algorithm: Option<String>,
    pub kdf_salt: Option<Vec<u8>>,
    pub kdf_m_cost: Option<i64>,
    pub kdf_t_cost: Option<i64>,
    pub kdf_p_cost: Option<i64>,
    pub cipher_algorithm: Option<String>,
    pub cipher_nonce: Option<Vec<u8>>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
