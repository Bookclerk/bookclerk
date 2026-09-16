//! Host-private resolved SQL v1 statement proof.
//!
//! Canonical [`crate::ExecuteRequest`] stays the public wire. Admission builds
//! one [`ResolvedStatement`] per statement; authorization, schema companions,
//! and adapter lowering consume it. This module is not a guest SDK surface.

use serde::{Deserialize, Serialize};

use super::sql_types::CreateTableSchema;
use super::{statement_sql_hash, SqlType};

/// Byte span in the exact canonical SQL string a proof is bound to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SqlSpan {
    /// Inclusive start byte offset.
    pub start: usize,
    /// Exclusive end byte offset.
    pub end: usize,
}

/// One TEXT expression that PostgreSQL must collate as `"C"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextCollateSite {
    /// Identifier or string-literal span in canonical SQL.
    pub span: SqlSpan,
}

/// INTEGER `+` `-` `*` `abs` site lowered to overflow → NULL.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum IntegerArithKind {
    /// `lhs + rhs`
    Add,
    /// `lhs - rhs`
    Sub,
    /// `lhs * rhs`
    Mul,
    /// `abs(arg)` (`lhs` is the argument span)
    Abs,
}

/// One INTEGER arithmetic expression that must not wrap or error on overflow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IntegerArithSite {
    /// Full expression span (`a + b` or `abs(n)`).
    pub full: SqlSpan,
    /// Left operand (or `abs` argument).
    pub lhs: SqlSpan,
    /// Right operand (`abs` repeats `lhs`).
    pub rhs: SqlSpan,
    /// Operator.
    pub kind: IntegerArithKind,
}

/// Column name recorded for `SELECT *` / `alias.*` / `RETURNING *` on a physical table.
pub const PHYSICAL_STAR_COLUMN: &str = "*";

/// Physical table/column access used for authorization.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PhysicalAccess {
    /// Folded physical table name.
    pub table: String,
    /// Folded column name when the access is column-specific.
    ///
    /// [`PHYSICAL_STAR_COLUMN`] (`"*"`) means a projection wildcard on this
    /// table (`SELECT *`, `alias.*`, `RETURNING *`). `None` is table presence
    /// only (FROM / INSERT / UPDATE / DELETE / DROP), not a wildcard.
    pub column: Option<String>,
}

/// Destination assignment `lhs = rhs` (INSERT, UPDATE, …).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedAssignment {
    /// Physical table of the destination column.
    pub table: String,
    /// Destination column.
    pub column: String,
    /// Destination type.
    pub dest: SqlType,
    /// Unified source type.
    pub source: SqlType,
}

/// CREATE/DROP action recorded on a proof.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SchemaAction {
    /// Not DDL.
    None,
    /// `CREATE TABLE` — `noop` when the durable fingerprint already matches.
    Create {
        /// Parsed SQL-v1 table schema (columns, nullability, identity, constraints).
        schema: Box<CreateTableSchema>,
        /// Structured schema fingerprint (hex SHA-256).
        fingerprint: String,
        /// True when catalog already has this exact fingerprint.
        noop: bool,
    },
    /// `DROP TABLE`.
    Drop {
        /// Folded table name.
        table: String,
    },
}

/// One resolved typed proof bound to an exact canonical statement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedStatement {
    /// SHA-256 hex of the exact canonical SQL this proof claims.
    pub statement_hash: String,
    /// SELECT / RETURNING / VALUES output columns in order.
    pub output_columns: Vec<(String, SqlType)>,
    /// Physical tables/columns referenced (authorization).
    pub physical_accesses: Vec<PhysicalAccess>,
    /// Mutation destination assignments.
    pub assignments: Vec<ResolvedAssignment>,
    /// TEXT expression spans for `COLLATE "C"` (already excluding dest/FROM/AS).
    pub text_collate_sites: Vec<TextCollateSite>,
    /// INTEGER overflow sites (`+` `-` `*` `abs`).
    #[serde(default)]
    pub integer_arith_sites: Vec<IntegerArithSite>,
    /// Function names invoked in code spans (folded), for authorization.
    #[serde(default)]
    pub functions: Vec<String>,
    /// DDL action + fingerprint.
    pub schema_action: SchemaAction,
}

impl ResolvedStatement {
    /// Proof for `sql` with empty semantic payloads (wrapper DDL/DML still hash-bound).
    #[must_use]
    pub fn bound_empty(sql: &str) -> Self {
        Self {
            statement_hash: statement_sql_hash(sql),
            output_columns: Vec::new(),
            physical_accesses: Vec::new(),
            assignments: Vec::new(),
            text_collate_sites: Vec::new(),
            integer_arith_sites: Vec::new(),
            functions: Vec::new(),
            schema_action: SchemaAction::None,
        }
    }

    /// True when `sql` is the exact statement this proof was built from.
    #[cfg(feature = "host")]
    #[must_use]
    pub fn proves(&self, sql: &str) -> bool {
        self.statement_hash == statement_sql_hash(sql)
    }

    /// Rejects a hash-correct proof whose TEXT/arithmetic sidecar cannot be applied.
    ///
    /// # Errors
    ///
    /// Returns when a span is empty, out of range, not on a UTF-8 boundary,
    /// operands are not contained in `full`, sites partially overlap, or an
    /// arithmetic kind does not match the operator in `sql`.
    pub fn validate_for(&self, sql: &str) -> crate::Result<()> {
        for site in &self.text_collate_sites {
            span_in_sql(sql, site.span, "TEXT collation")?;
        }
        for (i, site) in self.integer_arith_sites.iter().enumerate() {
            span_in_sql(sql, site.full, "INTEGER arithmetic")?;
            span_in_sql(sql, site.lhs, "INTEGER arithmetic lhs")?;
            span_in_sql(sql, site.rhs, "INTEGER arithmetic rhs")?;
            if !span_contains(site.full, site.lhs) || !span_contains(site.full, site.rhs) {
                return Err(crate::PluginError::internal(
                    "resolved SQL proof arithmetic operands are not inside the full site",
                ));
            }
            match site.kind {
                IntegerArithKind::Abs => {
                    let piece = sql[site.full.start..site.full.end].trim_start();
                    let head = piece.as_bytes().get(..3);
                    if !head.is_some_and(|h| h.eq_ignore_ascii_case(b"abs")) {
                        return Err(crate::PluginError::internal(
                            "resolved SQL proof abs site does not start with abs",
                        ));
                    }
                }
                IntegerArithKind::Add => {
                    if !binary_op_between(sql, site.lhs.end, site.rhs.start, '+') {
                        return Err(crate::PluginError::internal(
                            "resolved SQL proof arithmetic kind does not match the operator",
                        ));
                    }
                }
                IntegerArithKind::Sub => {
                    if !binary_op_between(sql, site.lhs.end, site.rhs.start, '-') {
                        return Err(crate::PluginError::internal(
                            "resolved SQL proof arithmetic kind does not match the operator",
                        ));
                    }
                }
                IntegerArithKind::Mul => {
                    if !binary_op_between(sql, site.lhs.end, site.rhs.start, '*') {
                        return Err(crate::PluginError::internal(
                            "resolved SQL proof arithmetic kind does not match the operator",
                        ));
                    }
                }
            }
            for (j, other) in self.integer_arith_sites.iter().enumerate() {
                if j <= i {
                    continue;
                }
                if !spans_nested_or_disjoint(site.full, other.full) {
                    return Err(crate::PluginError::internal(
                        "resolved SQL proof arithmetic sites partially overlap",
                    ));
                }
            }
        }
        Ok(())
    }
}

/// Require `proof` to be bound to `sql`.
///
/// # Errors
///
/// Returns when the hashes differ (transformed or reindexed SQL).
#[cfg(feature = "host")]
pub fn assert_proof_matches_sql(proof: &ResolvedStatement, sql: &str) -> crate::Result<()> {
    if proof.proves(sql) {
        Ok(())
    } else {
        Err(crate::PluginError::internal(
            "resolved SQL proof is not bound to this canonical statement",
        ))
    }
}

/// # Errors
///
/// Returns when the span is empty, past `sql`, or not on a UTF-8 boundary.
fn span_in_sql(sql: &str, span: SqlSpan, what: &str) -> crate::Result<()> {
    if span.start >= span.end
        || span.end > sql.len()
        || !sql.is_char_boundary(span.start)
        || !sql.is_char_boundary(span.end)
    {
        return Err(crate::PluginError::internal(format!(
            "resolved SQL proof {what} span is not a valid UTF-8 range in the statement"
        )));
    }
    Ok(())
}

/// True when `inner` lies inside `outer` (inclusive on both ends).
fn span_contains(outer: SqlSpan, inner: SqlSpan) -> bool {
    outer.start <= inner.start && inner.end <= outer.end
}

/// True when two spans nest or do not overlap (partial overlap is rejected).
fn spans_nested_or_disjoint(a: SqlSpan, b: SqlSpan) -> bool {
    a.end <= b.start || b.end <= a.start || span_contains(a, b) || span_contains(b, a)
}

/// True when `want` is the first non-whitespace operator between operands.
fn binary_op_between(sql: &str, lhs_end: usize, rhs_start: usize, want: char) -> bool {
    if lhs_end > rhs_start || rhs_start > sql.len() {
        return false;
    }
    sql[lhs_end..rhs_start].trim_start().starts_with(want)
}
