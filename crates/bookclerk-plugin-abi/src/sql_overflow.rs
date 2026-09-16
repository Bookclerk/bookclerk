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
/// After each wrap, remaining (outer) sites shift by the inserted byte count.
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
        if wrapped.len() < old_len {
            return Err(PluginError::internal(
                "INTEGER overflow wrap shrank the site",
            ));
        }
        let delta = wrapped.len() - old_len;
        let repl_end = site.full.end;
        out.replace_range(site.full.start..site.full.end, &wrapped);
        for later in sites.iter_mut().skip(i + 1) {
            later.full = shift_span(later.full, repl_end, delta);
            later.lhs = shift_span(later.lhs, repl_end, delta);
            later.rhs = shift_span(later.rhs, repl_end, delta);
        }
    }
    Ok(out)
}

pub(crate) fn shift_span(span: SqlSpan, repl_end: usize, delta: usize) -> SqlSpan {
    SqlSpan {
        start: if span.start >= repl_end {
            span.start.saturating_add(delta)
        } else {
            span.start
        },
        end: if span.end >= repl_end {
            span.end.saturating_add(delta)
        } else {
            span.end
        },
    }
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
}
