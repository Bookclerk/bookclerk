//! Shared migration plan types (host library and Bookclerk-owned binding bootstrap).

use bookclerk_plugin_abi::SqlTypeEnv;

use super::{migration_statements_checksum, prove_plan_sql_op, HostMigrationStep, MigrationOp};
use crate::error::{LibraryError, Result};
use crate::schema_state::{SCHEMA_STATE_FROZEN, SCHEMA_STATE_UNRELEASED};

/// Ledger namespace for host library schema and Bookclerk-owned binding bootstrap.
pub const BOOKCLERK_SCHEMA_NAMESPACE: &str = "bookclerk";

/// One admitted BookclerkSQL statement owned by a [`MigrationStep`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanOp {
    /// Schema mutation (`CREATE` / `DROP` / `ALTER` / index).
    Schema(String),
    /// Data backfill (`INSERT` / `UPDATE` / `DELETE`).
    Data(String),
}

impl PlanOp {
    /// Schema op from static host SQL.
    #[must_use]
    pub fn schema_static(sql: &'static str) -> Self {
        Self::Schema(sql.to_string())
    }

    /// Canonical BookclerkSQL for this op.
    #[must_use]
    pub fn sql(&self) -> &str {
        match self {
            Self::Schema(sql) | Self::Data(sql) => sql,
        }
    }

    /// True when this op is admitted schema DDL.
    #[must_use]
    pub fn is_schema(&self) -> bool {
        matches!(self, Self::Schema(_))
    }

    /// Converts a host static op into an owned plan op.
    #[must_use]
    pub fn from_static(op: MigrationOp) -> Self {
        match op {
            MigrationOp::Schema(sql) => Self::Schema(sql.to_string()),
            MigrationOp::Data(sql) => Self::Data(sql.to_string()),
        }
    }
}

/// One ordered schema revision: checksumed `up` ops and optional `down` ops.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationStep {
    /// Monotonic schema revision recorded on the namespaced ledger.
    pub version: i64,
    /// Ordered schema and data ops for this version.
    pub up: Vec<PlanOp>,
    /// Reverse ops when this step is reversible; `None` means restore a backup.
    pub down: Option<Vec<PlanOp>>,
    /// First package/semver that shipped this step.
    pub introduced_in: String,
}

impl MigrationStep {
    /// SHA-256 hex digest of length-prefixed `up` (and `down` when present).
    #[must_use]
    pub fn checksum(&self) -> String {
        plan_ops_checksum(&self.up, self.down.as_deref())
    }

    /// True when [`Self::down`] is present so an explicit downgrade can apply it.
    #[must_use]
    pub fn reversible(&self) -> bool {
        self.down.is_some()
    }

    /// Converts a static host step into an owned engine step.
    #[must_use]
    pub fn from_host(step: &HostMigrationStep) -> Self {
        Self {
            version: step.version,
            up: step
                .steps
                .iter()
                .copied()
                .map(PlanOp::from_static)
                .collect(),
            down: step
                .down
                .map(|ops| ops.iter().copied().map(PlanOp::from_static).collect()),
            introduced_in: step.introduced_in.to_string(),
        }
    }
}

/// Ordered frozen plan for one ledger namespace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationPlan {
    /// `bookclerk_schema_migrations.namespace` (`bookclerk` for host-owned plans).
    pub namespace: String,
    /// Frozen steps in increasing version order.
    pub steps: Vec<MigrationStep>,
}

impl MigrationPlan {
    /// Builds a plan after proving every op and checking version order.
    ///
    /// # Errors
    ///
    /// Returns when `namespace` is not a ledger namespace, versions are not
    /// strictly increasing positive integers, or an op fails BookclerkSQL proof.
    pub fn try_new(namespace: impl Into<String>, steps: Vec<MigrationStep>) -> Result<Self> {
        let namespace = namespace.into();
        validate_schema_namespace(&namespace)?;
        let mut prev = 0i64;
        for (index, step) in steps.iter().enumerate() {
            if step.version < 1 {
                return Err(LibraryError::Schema(format!(
                    "migration plan `{namespace}` step {index} version {} must be >= 1",
                    step.version
                )));
            }
            if step.version <= prev {
                return Err(LibraryError::Schema(format!(
                    "migration plan `{namespace}` versions must be strictly increasing \
                     (saw {} after {prev})",
                    step.version
                )));
            }
            if step.up.is_empty() {
                return Err(LibraryError::Schema(format!(
                    "migration plan `{namespace}` version {} has no up ops",
                    step.version
                )));
            }
            prove_plan_ops(&step.up)?;
            if let Some(down) = &step.down {
                prove_plan_ops(down)?;
            }
            prev = step.version;
        }
        Ok(Self { namespace, steps })
    }

    /// Oldest frozen version (`None` when this plan has no frozen steps).
    #[must_use]
    pub fn min_version(&self) -> Option<i64> {
        self.steps.first().map(|s| s.version)
    }

    /// Highest frozen version in this plan (`0` when empty).
    #[must_use]
    pub fn max_version(&self) -> i64 {
        self.steps.last().map(|s| s.version).unwrap_or(0)
    }

    /// Step with `version`, if present.
    #[must_use]
    pub fn step(&self, version: i64) -> Option<&MigrationStep> {
        self.steps.iter().find(|s| s.version == version)
    }

    /// Expected checksum map for frozen rows through `through`.
    #[must_use]
    pub fn checksums_through(&self, through: i64) -> std::collections::HashMap<i64, String> {
        self.steps
            .iter()
            .filter(|s| s.version <= through)
            .map(|s| (s.version, s.checksum()))
            .collect()
    }
}

/// SHA-256 of length-prefixed plan ops (not a joined script).
#[must_use]
pub fn plan_ops_checksum(ups: &[PlanOp], down: Option<&[PlanOp]>) -> String {
    let up: Vec<&str> = ups.iter().map(PlanOp::sql).collect();
    let down_sql: Option<Vec<&str>> = down.map(|ops| ops.iter().map(PlanOp::sql).collect());
    let down_refs: Option<Vec<&str>> = down_sql.as_ref().map(|v| v.to_vec());
    migration_statements_checksum(&up, down_refs.as_deref())
}

/// Proves each owned op is exactly one BookclerkSQL statement of the declared kind.
///
/// # Errors
///
/// Returns when packing, kind, or typechecking fails.
pub fn prove_plan_ops(ops: &[PlanOp]) -> Result<()> {
    let mut env = SqlTypeEnv::new();
    for (index, op) in ops.iter().enumerate() {
        prove_plan_sql_op(index, op.sql(), op.is_schema(), &mut env)?;
    }
    Ok(())
}

/// True when `namespace` is `bookclerk` or a plugin-id grammar string.
///
/// # Errors
///
/// Returns when the namespace is empty, too long, or contains characters other
/// than lowercase ascii, digits, and single `_`.
pub fn validate_schema_namespace(namespace: &str) -> Result<()> {
    if namespace != namespace.trim() {
        return Err(LibraryError::Schema(format!(
            "schema namespace `{namespace}` must not have leading or trailing whitespace"
        )));
    }
    if namespace.len() < 2 || namespace.len() > 32 {
        return Err(LibraryError::Schema(format!(
            "schema namespace `{namespace}` must be 2–32 characters"
        )));
    }
    if !namespace
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    {
        return Err(LibraryError::Schema(format!(
            "schema namespace `{namespace}` must be lowercase ascii letters, digits, or `_`"
        )));
    }
    if namespace.starts_with('_') || namespace.ends_with('_') || namespace.contains("__") {
        return Err(LibraryError::Schema(format!(
            "schema namespace `{namespace}` must not start/end with `_` or contain `__`"
        )));
    }
    Ok(())
}

/// SQL string literal with `'` doubled.
#[must_use]
pub fn sql_string_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// `INSERT` for a frozen namespaced `bookclerk_schema_migrations` row.
#[must_use]
pub fn frozen_marker_sql(namespace: &str, version: i64, checksum: &str) -> String {
    marker_insert_sql(namespace, version, SCHEMA_STATE_FROZEN, checksum)
}

/// `INSERT` for an unreleased namespaced `bookclerk_schema_migrations` row.
#[must_use]
pub fn unreleased_marker_sql_in(namespace: &str, checksum: &str, base_version: i64) -> String {
    marker_insert_sql(namespace, base_version, SCHEMA_STATE_UNRELEASED, checksum)
}

/// Namespaced `bookclerk_schema_migrations` INSERT for frozen or unreleased markers.
fn marker_insert_sql(namespace: &str, version: i64, state: &str, checksum: &str) -> String {
    let app = env!("CARGO_PKG_VERSION").replace('\'', "''");
    let at = chrono::Utc::now().to_rfc3339().replace('\'', "''");
    let ns = sql_string_literal(namespace);
    let checksum = sql_string_literal(checksum);
    format!(
        "INSERT INTO bookclerk_schema_migrations \
         (namespace, version, state, checksum, app_version, applied_at) \
         VALUES ({ns}, {version}, '{state}', {checksum}, '{app}', '{at}')"
    )
}

/// `DELETE` of one frozen step's namespaced marker.
#[must_use]
pub fn frozen_marker_delete_sql(namespace: &str, version: i64) -> String {
    format!(
        "DELETE FROM bookclerk_schema_migrations WHERE namespace = {} AND state = '{}' AND version = {version}",
        sql_string_literal(namespace),
        SCHEMA_STATE_FROZEN
    )
}

/// Slot key that serializes schema apply for one namespace.
#[must_use]
pub fn schema_slot_key(namespace: &str) -> String {
    format!("schema:{namespace}")
}
