//! Proof-directed INTEGER overflow CASE wraps shared by sqlite-family
//! adapters and portable D1 physical preflight.
//!
//! Templates must stay identical to the sqlite/postgres lowering in
//! `bookclerk-db-exec`. Mul wraps contain `/`, so mechanical `NULLIF`
//! rewriting is applied **after** these wraps.

#![allow(clippy::missing_docs_in_private_items)]

use crate::sql_proof::{IntegerArithKind, IntegerArithSite, SqlSpan};
use crate::{PluginError, Result};

/// i64::MAX as a portable SQL integer.
pub const I64_MAX_SQL: &str = "9223372036854775807";

/// Engine family for overflow derived-table syntax and i64::MIN CAST.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverflowDialect {
    /// SQLite / D1: correlated `FROM (SELECT …) _bc_ov`, `INTEGER` min CAST.
    SqliteFamily,
    /// PostgreSQL: `LATERAL` + `BIGINT` min CAST.
    #[allow(dead_code)]
    Postgres,
}

impl OverflowDialect {
    fn i64_min_sql(self) -> &'static str {
        match self {
            Self::Postgres => "CAST('-9223372036854775808' AS BIGINT)",
            Self::SqliteFamily => "CAST('-9223372036854775808' AS INTEGER)",
        }
    }

    fn row_source(self, cols: &str) -> String {
        match self {
            Self::Postgres => format!("LATERAL (SELECT {cols}) _bc_ov"),
            Self::SqliteFamily => format!("(SELECT {cols}) _bc_ov"),
        }
    }
}

/// Wrap each INTEGER `+`/`-`/`*`/`abs` site in a portable overflow CASE.
///
/// Sites are applied **innermost-first** (`full.end`, then reverse `full.start`)
/// so right-nested `1 + abs(n)` rewrites the `abs` call before the outer add.
/// After each wrap, remaining (outer) sites shift by the byte count the wrap
/// added or removed; a wrap drops comments between operands, so it can be
/// shorter than its site.
///
/// # Errors
///
/// Returns when a site is empty, past `sql`, not on a UTF-8 boundary, or cannot
/// be wrapped as the recorded operator.
pub fn apply_integer_overflow(
    dialect: OverflowDialect,
    sql: &str,
    sites: &[IntegerArithSite],
) -> Result<String> {
    let mut sites = sites.to_vec();
    sites.sort_by_key(|s| (s.full.end, std::cmp::Reverse(s.full.start)));
    let mut out = sql.to_string();
    for i in 0..sites.len() {
        let site = sites[i];
        if site.full.start >= site.full.end || site.full.end > out.len() {
            return Err(PluginError::internal(
                "INTEGER overflow site is empty or past end of SQL",
            ));
        }
        if !out.is_char_boundary(site.full.start) || !out.is_char_boundary(site.full.end) {
            return Err(PluginError::internal(
                "INTEGER overflow site is not on a UTF-8 boundary",
            ));
        }
        let wrapped = wrap_integer_arith(dialect, &out, &site)?;
        let old_len = site.full.end - site.full.start;
        let repl_end = site.full.end;
        out.replace_range(site.full.start..site.full.end, &wrapped);
        for later in sites.iter_mut().skip(i + 1) {
            later.full = shift_span(later.full, repl_end, old_len, wrapped.len())?;
            later.lhs = shift_span(later.lhs, repl_end, old_len, wrapped.len())?;
            later.rhs = shift_span(later.rhs, repl_end, old_len, wrapped.len())?;
        }
    }
    Ok(out)
}

/// Moves a span boundary past a replaced range to its offset in the new SQL.
///
/// A wrap renders only the operands, so it can be shorter than the site it
/// replaces when the site holds comments or whitespace between them
/// (`id --<long comment>\n- 1`). Boundaries at or past `repl_end` move by
/// `new_len - old_len` in either direction; earlier ones stay. Outer sites are
/// applied after inner ones, so their boundaries never fall strictly inside
/// the replaced range.
///
/// # Arguments
///
/// * `span` - a later site's span in the SQL before the replacement.
/// * `repl_end` - byte offset where the replaced range ended.
/// * `old_len` - byte length of the replaced range.
/// * `new_len` - byte length of its replacement.
///
/// # Errors
///
/// Returns when a moved boundary would underflow or overflow `usize`.
fn shift_span(span: SqlSpan, repl_end: usize, old_len: usize, new_len: usize) -> Result<SqlSpan> {
    let shift = |pos: usize| -> Result<usize> {
        if pos < repl_end {
            return Ok(pos);
        }
        pos.checked_sub(old_len)
            .and_then(|base| base.checked_add(new_len))
            .ok_or_else(|| PluginError::internal("INTEGER overflow site shift is out of range"))
    };
    Ok(SqlSpan {
        start: shift(span.start)?,
        end: shift(span.end)?,
    })
}

/// Renders one INTEGER overflow CASE wrap for `site`.
///
/// # Errors
///
/// Returns when `site` is not a valid UTF-8 range in `sql`, or an `abs` site
/// is not a call.
fn wrap_integer_arith(
    dialect: OverflowDialect,
    sql: &str,
    site: &IntegerArithSite,
) -> Result<String> {
    if site.full.end > sql.len()
        || site.lhs.end > sql.len()
        || site.rhs.end > sql.len()
        || site.lhs.start >= site.lhs.end
        || site.full.start >= site.full.end
        || !sql.is_char_boundary(site.full.start)
        || !sql.is_char_boundary(site.full.end)
        || !sql.is_char_boundary(site.lhs.start)
        || !sql.is_char_boundary(site.lhs.end)
        || !sql.is_char_boundary(site.rhs.start)
        || !sql.is_char_boundary(site.rhs.end)
    {
        return Err(PluginError::internal(
            "INTEGER overflow site is not a valid UTF-8 range in the statement",
        ));
    }
    let min = dialect.i64_min_sql();
    match site.kind {
        IntegerArithKind::Abs => {
            let full = &sql[site.full.start..site.full.end];
            let arg = abs_call_arg(full)
                .ok_or_else(|| PluginError::internal("INTEGER overflow abs site is not a call"))?;
            let src = dialect.row_source(&format!("({arg}) AS a"));
            Ok(format!(
                "(SELECT CASE WHEN a IS NULL THEN NULL WHEN a = {min} THEN NULL ELSE abs(a) END \
                 FROM {src})"
            ))
        }
        IntegerArithKind::Add => {
            let a = &sql[site.lhs.start..site.lhs.end];
            let b = &sql[site.rhs.start..site.rhs.end];
            let src = dialect.row_source(&format!("({a}) AS a, ({b}) AS b"));
            Ok(format!(
                "(SELECT CASE WHEN a IS NULL OR b IS NULL THEN a + b \
                 WHEN a > 0 AND b > 0 AND a > {I64_MAX_SQL} - b THEN NULL \
                 WHEN a < 0 AND b < 0 AND a < {min} - b THEN NULL \
                 ELSE a + b END FROM {src})"
            ))
        }
        IntegerArithKind::Sub => {
            let a = &sql[site.lhs.start..site.lhs.end];
            let b = &sql[site.rhs.start..site.rhs.end];
            let src = dialect.row_source(&format!("({a}) AS a, ({b}) AS b"));
            Ok(format!(
                "(SELECT CASE WHEN a IS NULL OR b IS NULL THEN a - b \
                 WHEN b < 0 AND a > {I64_MAX_SQL} + b THEN NULL \
                 WHEN b > 0 AND a < {min} + b THEN NULL \
                 ELSE a - b END FROM {src})"
            ))
        }
        IntegerArithKind::Mul => {
            let a = &sql[site.lhs.start..site.lhs.end];
            let b = &sql[site.rhs.start..site.rhs.end];
            let src = dialect.row_source(&format!("({a}) AS a, ({b}) AS b"));
            Ok(format!(
                "(SELECT CASE WHEN a IS NULL OR b IS NULL THEN a * b \
                 WHEN a = 0 OR b = 0 THEN 0 \
                 WHEN (a = {min} AND b = -1) OR (b = {min} AND a = -1) THEN NULL \
                 WHEN b = -1 THEN (0 - a) \
                 WHEN b > 0 AND (a > {I64_MAX_SQL} / b OR a < {min} / b) THEN NULL \
                 WHEN b < 0 AND (a < {I64_MAX_SQL} / b OR a > {min} / b) THEN NULL \
                 ELSE a * b END FROM {src})"
            ))
        }
    }
}

fn abs_call_arg(full: &str) -> Option<&str> {
    let open = full.as_bytes().iter().position(|b| *b == b'(')?;
    let close = full.as_bytes().iter().rposition(|b| *b == b')')?;
    if close <= open {
        return None;
    }
    Some(full[open + 1..close].trim())
}

#[cfg(test)]
#[allow(clippy::missing_panics_doc)]
mod tests {
    use super::*;
    use crate::sql_proof::{IntegerArithKind, SqlSpan};

    #[test]
    fn sqlite_and_postgres_wraps_differ_on_row_source() {
        let sql = "SELECT 1 + 2";
        let site = IntegerArithSite {
            full: SqlSpan { start: 7, end: 12 },
            lhs: SqlSpan { start: 7, end: 8 },
            rhs: SqlSpan { start: 11, end: 12 },
            kind: IntegerArithKind::Add,
        };
        let sqlite = apply_integer_overflow(OverflowDialect::SqliteFamily, sql, &[site]).unwrap();
        let postgres = apply_integer_overflow(OverflowDialect::Postgres, sql, &[site]).unwrap();
        assert!(
            sqlite.contains("(SELECT (1) AS a, (2) AS b) _bc_ov"),
            "{sqlite}"
        );
        assert!(
            postgres.contains("LATERAL (SELECT (1) AS a, (2) AS b) _bc_ov"),
            "{postgres}"
        );
        assert!(postgres.contains("BIGINT"), "{postgres}");
        assert!(sqlite.contains("INTEGER"), "{sqlite}");
    }

    #[test]
    fn shorter_wrap_shifts_enclosing_site_back() {
        // Found by the guest_sql_authz fuzz target: a long `--` comment between
        // the operands of an INTEGER `-` made the wrap shorter than its site,
        // and the typed lowering refused an admitted statement.
        let comment = "x".repeat(400);
        let inner = format!("id --{comment}\n- 1");
        let sql = format!("SELECT ({inner}) + 2");
        let inner_start = "SELECT (".len();
        let inner_end = inner_start + inner.len();
        let sub = IntegerArithSite {
            full: SqlSpan {
                start: inner_start,
                end: inner_end,
            },
            lhs: SqlSpan {
                start: inner_start,
                end: inner_start + 2,
            },
            rhs: SqlSpan {
                start: inner_end - 1,
                end: inner_end,
            },
            kind: IntegerArithKind::Sub,
        };
        let add = IntegerArithSite {
            full: SqlSpan {
                start: inner_start - 1,
                end: sql.len(),
            },
            lhs: SqlSpan {
                start: inner_start - 1,
                end: inner_end + 1,
            },
            rhs: SqlSpan {
                start: sql.len() - 1,
                end: sql.len(),
            },
            kind: IntegerArithKind::Add,
        };
        let out = apply_integer_overflow(OverflowDialect::SqliteFamily, &sql, &[add, sub]).unwrap();
        assert!(!out.contains(&comment), "{out}");
        assert!(
            out.starts_with("SELECT (SELECT CASE WHEN a IS NULL OR b IS NULL THEN a + b"),
            "{out}"
        );
        assert!(out.contains("(SELECT (id) AS a, (1) AS b) _bc_ov"), "{out}");
        assert!(out.ends_with("AS a, (2) AS b) _bc_ov)"), "{out}");
        assert_eq!(out.matches("_bc_ov").count(), 2, "{out}");
    }
}
