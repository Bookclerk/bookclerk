//! Canonical Bookclerk SQL → engine lowering (provider-SDK / adapter boundary).
//!
//! Host domain plans emit SQLite-shaped canonical SQL (`?`, `INSERT OR IGNORE`,
//! `json_extract`, `json_valid`). Postgres adapters apply this table before
//! execution. Rewrites are SQL-token aware: quoted strings/identifiers, line
//! and block comments, and PostgreSQL dollar quotes are copied verbatim.
//! Do not call this from host domain compilers.

#![allow(clippy::missing_docs_in_private_items)]

use bookclerk_plugin_abi::{
    apply_integer_overflow, assert_proof_matches_sql, IntegerArithSite, OverflowDialect,
    PluginError, ResolvedStatement, SqlSpan, INSERT_SELECT_WRAP_ALIAS, LIKE_GLOB_WRAP_PREFIX,
    LIKE_GLOB_WRAP_SUFFIX,
};
use sea_orm::DatabaseBackend;

/// Lowers canonical Bookclerk **DML/query** SQL for `backend`.
///
/// Every backend rewrites `INSERT OR IGNORE` to unique/PK `ON CONFLICT DO
/// NOTHING` (SQLite `OR IGNORE` would otherwise swallow `NOT NULL`). Postgres
/// adapters then rewrite helpers (`IFNULL`, `json_object` → TEXT,
/// `json_extract`, 2+-arg `min`/`max`, `json_valid`, `round`/`sum`/`avg`)
/// and `?` placeholders. Default `ORDER BY` NULLS and `/` `%` by-zero
/// `NULLIF` are host semantic desugars, not adapter rewrites. Binding and host
/// **DDL** type/identity rewrites
/// (`AUTOINCREMENT`, `BLOB`, `INTEGER`) stay on the adapter execution edge
/// ([`crate::schema_sql_for_backend`], [`crate::lower_binding_ddl_execute_request`])
/// so this function does not classify statements.
#[must_use]
pub fn lower_canonical_sql(backend: DatabaseBackend, sql: &str) -> String {
    lower_mechanical(backend, sql.to_string())
}

/// [`lower_canonical_sql`] plus proof-directed overflow and Postgres `COLLATE "C"`.
///
/// `proof` must be bound to `sql` (the canonical string before mechanical
/// lowering). When absent, TEXT collation and INTEGER overflow wraps are not
/// applied. TEXT collation runs **before** INTEGER overflow wrapping so a
/// TEXT span nested in `length(name) + 1` is remapped into the overflow
/// operands. Overflow sites are applied innermost-first.
///
/// # Errors
///
/// Returns when `proof` is present and not bound to `sql`, or when a
/// hash-correct sidecar cannot be applied (`ResolvedStatement::validate_for`).
pub fn lower_canonical_sql_typed(
    backend: DatabaseBackend,
    sql: &str,
    proof: Option<&ResolvedStatement>,
) -> Result<String, bookclerk_plugin_abi::PluginError> {
    let mut sql = sql.to_string();
    if let Some(proof) = proof {
        assert_proof_matches_sql(proof, &sql)?;
        proof.validate_for(&sql)?;
        let mut arith = proof.integer_arith_sites.clone();
        let collate: Vec<SqlSpan> = proof.text_collate_sites.iter().map(|s| s.span).collect();
        if backend == DatabaseBackend::Postgres {
            sql = apply_text_collate_spans(&sql, &collate, &mut arith)?;
        }
        sql = apply_integer_overflow(overflow_dialect(backend), &sql, &arith)?;
    }
    Ok(lower_mechanical(backend, sql))
}

fn lower_mechanical(backend: DatabaseBackend, sql: String) -> String {
    let sql = rewrite_insert_or_ignore_unique_conflict(&sql);
    if backend != DatabaseBackend::Postgres {
        return rewrite_like_to_glob(&sql);
    }
    lower_canonical_to_postgres_helpers(&sql)
}

/// Lowers canonical SQLite-shaped SQL onto PostgreSQL.
#[must_use]
pub fn lower_canonical_to_postgres(sql: &str) -> String {
    let sql = rewrite_insert_or_ignore_unique_conflict(sql);
    lower_canonical_to_postgres_helpers(&sql)
}

/// Postgres helper / placeholder rewrites (after unique-conflict INSERT).
///
/// Default `ORDER BY` NULLS and `/` `%` by-zero `NULLIF` are host semantic
/// desugars ([`bookclerk_plugin_abi::desugar_canonical_sql`]), not adapter
/// dialect generation.
fn lower_canonical_to_postgres_helpers(sql: &str) -> String {
    let sql = sqlite_fns_to_postgres(sql);
    rewrite_placeholders_postgres(&sql)
}

/// Rewrites SQLite `?` placeholders to Postgres `$1`…`$n` (code spans only).
fn rewrite_placeholders_postgres(sql: &str) -> String {
    let mut n = 0u32;
    let mut out = String::with_capacity(sql.len() + 16);
    let mut i = 0;
    while i < sql.len() {
        if let Some(len) = literal_or_comment_len(&sql[i..]) {
            out.push_str(&sql[i..i + len]);
            i += len;
            continue;
        }
        let ch = sql[i..].chars().next().unwrap_or('\0');
        if ch == '?' {
            n += 1;
            out.push('$');
            out.push_str(&n.to_string());
            i += 1;
        } else {
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

/// Renders `INSERT OR IGNORE` as unique/PK `ON CONFLICT DO NOTHING` (before `RETURNING`).
///
/// Canonical v1 conflict domain is uniqueness only: `NOT NULL` / `CHECK` still
/// abort. SQLite `OR IGNORE` would otherwise swallow those. The
/// `INSERT OR IGNORE INTO` prefix and the `RETURNING` keyword are matched
/// case-insensitively in code spans. [`find_last_in_code`] stays case-sensitive so
/// the host write-gate constant is never rewritten by accident.
fn rewrite_insert_or_ignore_unique_conflict(sql: &str) -> String {
    let trimmed = skip_trivia(sql);
    let Some(rest) = strip_prefix_ci(trimmed, "INSERT OR IGNORE INTO") else {
        return sql.to_string();
    };
    let prefix_len = sql.len() - trimmed.len();
    let rebuilt = if let Some(idx) = find_word_in_code_ci(rest, "RETURNING") {
        let head = wrap_insert_select_head(rest[..idx].trim_end());
        let after = idx + "RETURNING".len();
        format!(
            "INSERT INTO{head} ON CONFLICT DO NOTHING RETURNING{}",
            &rest[after..]
        )
    } else {
        format!(
            "INSERT INTO{} ON CONFLICT DO NOTHING",
            wrap_insert_select_head(rest)
        )
    };
    let mut out = String::with_capacity(prefix_len + rebuilt.len());
    out.push_str(&sql[..prefix_len]);
    out.push_str(&rebuilt);
    out
}

/// Wraps a `SELECT`/`WITH` insert source so `ON CONFLICT` is unambiguous.
///
/// `VALUES` stays unwrapped. Compound queries, `ORDER BY`, and `LIMIT` stay
/// inside the subquery. Guests cannot name [`INSERT_SELECT_WRAP_ALIAS`].
fn wrap_insert_select_head(head: &str) -> String {
    let Some(src_off) = insert_row_source_offset(head) else {
        return head.to_string();
    };
    let source = head[src_off..].trim();
    if ident_eq_ci(source, 0, "SELECT") || ident_eq_ci(source, 0, "WITH") {
        let prefix = &head[..src_off];
        format!("{prefix}SELECT * FROM ({source}) AS {INSERT_SELECT_WRAP_ALIAS} WHERE true ")
    } else {
        head.to_string()
    }
}

/// Byte offset of `VALUES` / `SELECT` / `WITH` after `INSERT INTO table [(cols)]`.
fn insert_row_source_offset(head: &str) -> Option<usize> {
    let mut i = skip_trivia_idx(head, 0);
    let (_, end) = ident_span_at(head, i)?;
    i = skip_trivia_idx(head, end);
    if head.as_bytes().get(i) == Some(&b'(') {
        i = skip_balanced(head, i);
        i = skip_trivia_idx(head, i);
    }
    Some(i)
}

/// SQLite/D1: case-sensitive `LIKE`/`NOT LIKE` via `GLOB` and bind-safe `replace`.
///
/// Escapes GLOB metacharacters `[` `*` `?` first, then maps `%`→`*` and `_`→`?`.
fn rewrite_like_to_glob(sql: &str) -> String {
    let mut i = 0;
    let mut out = String::with_capacity(sql.len() + 64);
    while i < sql.len() {
        let Some(rest) = sql.get(i..) else {
            break;
        };
        if let Some(len) = literal_or_comment_len(rest) {
            out.push_str(&rest[..len]);
            i += len;
            continue;
        }
        if ident_eq_ci(sql, i, "NOT") {
            let after_not = skip_trivia_idx(sql, i + 3);
            if ident_eq_ci(sql, after_not, "LIKE") {
                out.push_str("NOT GLOB ");
                i = skip_trivia_idx(sql, after_not + 4);
                let end = like_pattern_end(sql, i);
                out.push_str(&glob_pattern_sql(sql.get(i..end).unwrap_or("")));
                i = end;
                continue;
            }
        }
        if ident_eq_ci(sql, i, "LIKE") {
            out.push_str("GLOB ");
            i = skip_trivia_idx(sql, i + 4);
            let end = like_pattern_end(sql, i);
            out.push_str(&glob_pattern_sql(sql.get(i..end).unwrap_or("")));
            i = end;
            continue;
        }
        let ch = rest.chars().next().unwrap_or('\0');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Bind-safe GLOB conversion of a SQL v1 `LIKE` pattern expression.
fn glob_pattern_sql(pat: &str) -> String {
    format!("{LIKE_GLOB_WRAP_PREFIX}{pat}{LIKE_GLOB_WRAP_SUFFIX}")
}

/// End offset of the `LIKE` pattern expression starting at `start`.
fn like_pattern_end(sql: &str, start: usize) -> usize {
    let mut i = skip_sql_atom(sql, start);
    loop {
        let j = skip_trivia_idx(sql, i);
        if sql.get(j..).is_some_and(|s| s.starts_with("||")) {
            i = skip_sql_atom(sql, j + 2);
            continue;
        }
        break;
    }
    i
}

/// Skips one SQL atom (paren group, literal, bind, ident/call).
fn skip_sql_atom(sql: &str, start: usize) -> usize {
    let mut i = skip_trivia_idx(sql, start);
    if sql.as_bytes().get(i) == Some(&b'(') {
        return skip_balanced(sql, i);
    }
    if let Some(len) = sql.get(i..).and_then(literal_or_comment_len) {
        return i + len;
    }
    if sql.as_bytes().get(i) == Some(&b'?') {
        return i + 1;
    }
    if ident_eq_ci(sql, i, "NULL") {
        return i + 4;
    }
    if let Some((_, end)) = ident_span_at(sql, i) {
        let j = skip_trivia_idx(sql, end);
        if sql.as_bytes().get(j) == Some(&b'(') {
            return skip_balanced(sql, j);
        }
        if sql.as_bytes().get(j) == Some(&b'.') {
            let k = skip_trivia_idx(sql, j + 1);
            if let Some((_, e2)) = ident_span_at(sql, k) {
                let m = skip_trivia_idx(sql, e2);
                if sql.as_bytes().get(m) == Some(&b'(') {
                    return skip_balanced(sql, m);
                }
                return e2;
            }
        }
        return end;
    }
    if sql
        .as_bytes()
        .get(i)
        .is_some_and(|b| b.is_ascii_digit() || *b == b'-' || *b == b'+')
    {
        let start = i;
        if sql.as_bytes().get(i) == Some(&b'-') || sql.as_bytes().get(i) == Some(&b'+') {
            i += 1;
        }
        while i < sql.len() && sql.as_bytes()[i].is_ascii_digit() {
            i += 1;
        }
        if sql.as_bytes().get(i) == Some(&b'.') {
            i += 1;
            while i < sql.len() && sql.as_bytes()[i].is_ascii_digit() {
                i += 1;
            }
        }
        return if i > start { i } else { start };
    }
    i
}

/// Index after a balanced `(…)` group starting at `open` (`(`).
fn skip_balanced(sql: &str, open: usize) -> usize {
    let mut depth = 0i32;
    let mut i = open;
    while i < sql.len() {
        if let Some(len) = literal_or_comment_len(&sql[i..]) {
            i += len;
            continue;
        }
        let ch = sql[i..].chars().next().unwrap_or('\0');
        if ch == '(' {
            depth += 1;
        } else if ch == ')' {
            depth -= 1;
            if depth == 0 {
                return i + 1;
            }
        }
        i += ch.len_utf8();
    }
    sql.len()
}

/// Wrap each TEXT span as `(piece COLLATE "C")` (Postgres).
///
/// Applied **before** INTEGER overflow wrapping. Spans are applied
/// right-to-left. Trailing trivia on a TEXT span is left outside the wrap;
/// INTEGER sites shift from the trimmed end.
///
/// JSON `json_extract` path literals are **not** skipped by contents here. The
/// typechecker omits those path args from `text_collate_sites`
/// (`suppress_string_collate`) so a comparison / `ORDER BY` on a `'$.…'`
/// string still gets `"C"`.
///
/// # Errors
///
/// Returns when a span is empty, past `sql`, or not on a UTF-8 boundary.
fn apply_text_collate_spans(
    sql: &str,
    sites: &[SqlSpan],
    arith: &mut [IntegerArithSite],
) -> Result<String, PluginError> {
    let mut ordered = sites.to_vec();
    ordered.sort_by_key(|s| std::cmp::Reverse(s.start));
    let mut out = sql.to_string();
    for span in ordered {
        if span.start >= span.end || span.end > out.len() {
            return Err(PluginError::internal(
                "text collate span is empty or past end of SQL",
            ));
        }
        if !out.is_char_boundary(span.start) || !out.is_char_boundary(span.end) {
            return Err(PluginError::internal(
                "text collate span is not on a UTF-8 boundary",
            ));
        }
        let raw = &out[span.start..span.end];
        // TypeCx expression walks skip trailing trivia into some TEXT spans
        // (`infer_add` looks ahead for `+`). Wrap the trimmed ident/literal so
        // `SELECT body FROM` stays `SELECT (body COLLATE "C") FROM`.
        let piece = raw.trim_end();
        if piece.is_empty() {
            return Err(PluginError::internal("text collate span is empty"));
        }
        let trim_end = span.start + piece.len();
        if !out.is_char_boundary(trim_end) {
            return Err(PluginError::internal(
                "text collate span trim is not on a UTF-8 boundary",
            ));
        }
        let wrapped = format!("({piece} COLLATE \"C\")");
        let old_len = piece.len();
        if wrapped.len() < old_len {
            return Err(PluginError::internal("text collate wrap shrank the span"));
        }
        let delta = wrapped.len() - old_len;
        out.replace_range(span.start..trim_end, &wrapped);
        for site in arith.iter_mut() {
            site.full = shift_span(site.full, trim_end, delta);
            site.lhs = shift_span(site.lhs, trim_end, delta);
            site.rhs = shift_span(site.rhs, trim_end, delta);
        }
    }
    Ok(out)
}

fn shift_span(span: SqlSpan, repl_end: usize, delta: usize) -> SqlSpan {
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

fn overflow_dialect(backend: DatabaseBackend) -> OverflowDialect {
    if backend == DatabaseBackend::Postgres {
        OverflowDialect::Postgres
    } else {
        OverflowDialect::SqliteFamily
    }
}

/// Maps SQLite helpers used in host plans onto PostgreSQL equivalents.
fn sqlite_fns_to_postgres(sql: &str) -> String {
    let mut sql = rewrite_fn_name(sql, "ifnull", "COALESCE");
    sql = rewrite_json_object(&sql);
    sql = rewrite_variadic_min_max(&sql);
    sql = rewrite_round_sum_avg(&sql);
    sql = rewrite_json_valid(&sql);
    sql = rewrite_json_extract(&sql);
    sql = replace_in_code(
        &sql,
        "json(payload)",
        "(CASE WHEN payload IS JSON THEN payload::jsonb END)",
    );
    sql = replace_in_code(
        &sql,
        "json(CASE WHEN password_hash IS NOT NULL AND password_hash != '' THEN 'true' ELSE 'false' END)",
        "(password_hash IS NOT NULL AND password_hash != '')",
    );
    sql = replace_in_code(
        &sql,
        "json(CASE WHEN cancel_requested != 0 THEN 'true' ELSE 'false' END)",
        "(cancel_requested != 0)",
    );
    sql = replace_in_code(
        &sql,
        "json(CASE WHEN resume_pending != 0 THEN 'true' ELSE 'false' END)",
        "(resume_pending != 0)",
    );
    rewrite_julianday_delta(&sql)
}

/// Replaces a function ident (`name(`) with `pg_name(` (case-insensitive).
fn rewrite_fn_name(sql: &str, name: &str, pg_name: &str) -> String {
    let mut i = 0;
    let mut out = String::with_capacity(sql.len() + pg_name.len());
    while i < sql.len() {
        if let Some(len) = literal_or_comment_len(&sql[i..]) {
            out.push_str(&sql[i..i + len]);
            i += len;
            continue;
        }
        if ident_call_at(sql, i, name) {
            out.push_str(pg_name);
            i += name.len();
            continue;
        }
        let ch = sql[i..].chars().next().unwrap_or('\0');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Maps `json_object` onto `json_build_object` and the SQL-v1 TEXT wire type.
///
/// Postgres `json_build_object` returns `json`, so `length(json_object(…))`
/// would fail (`length(json)` does not exist). Nested `json_object` values stay
/// `json` so they nest as objects rather than JSON strings.
fn rewrite_json_object(sql: &str) -> String {
    rewrite_json_object_calls(sql, false)
}

fn rewrite_json_object_calls(sql: &str, keep_json: bool) -> String {
    let mut i = 0;
    let mut out = String::with_capacity(sql.len() + 16);
    while i < sql.len() {
        if let Some(len) = literal_or_comment_len(&sql[i..]) {
            out.push_str(&sql[i..i + len]);
            i += len;
            continue;
        }
        if ident_call_at(sql, i, "json_object") {
            let open = sql[i + "json_object".len()..]
                .char_indices()
                .find(|(_, c)| !c.is_whitespace())
                .map(|(off, _)| i + "json_object".len() + off)
                .unwrap_or(i + "json_object".len());
            if let Some((args, rest)) = split_call_args(&sql[open + 1..]) {
                let rewritten: Vec<String> = args
                    .iter()
                    .map(|a| {
                        let nested = is_sole_json_object_call(a);
                        rewrite_json_object_calls(a, nested)
                    })
                    .collect();
                let call = format!("json_build_object({})", rewritten.join(", "));
                if keep_json {
                    out.push_str(&call);
                } else {
                    out.push('(');
                    out.push_str(&call);
                    out.push_str("::text)");
                }
                i = sql.len() - rest.len();
                continue;
            }
        }
        let ch = sql[i..].chars().next().unwrap_or('\0');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// True when `s` is exactly one `json_object(…)` call (optional outer space).
fn is_sole_json_object_call(s: &str) -> bool {
    let s = s.trim();
    if !ident_call_at(s, 0, "json_object") {
        return false;
    }
    let open = s["json_object".len()..]
        .char_indices()
        .find(|(_, c)| !c.is_whitespace())
        .map(|(off, _)| "json_object".len() + off)
        .unwrap_or("json_object".len());
    matches!(
        split_call_args(&s[open + 1..]),
        Some((_, rest)) if rest.trim().is_empty()
    )
}

/// Rewrites 2+-arg `min`/`max` (SQLite scalars) to `LEAST`/`GREATEST`.
///
/// One-argument `MIN`/`MAX` stay aggregates. Nested calls are rewritten
/// inside-out.
fn rewrite_variadic_min_max(sql: &str) -> String {
    let sql = rewrite_variadic_fn(sql, "min", "LEAST");
    rewrite_variadic_fn(&sql, "max", "GREATEST")
}

/// Rewrites `name(...)` with two or more arguments to `pg_name(...)`.
fn rewrite_variadic_fn(sql: &str, name: &str, pg_name: &str) -> String {
    let mut i = 0;
    let mut out = String::with_capacity(sql.len());
    while i < sql.len() {
        if let Some(len) = literal_or_comment_len(&sql[i..]) {
            out.push_str(&sql[i..i + len]);
            i += len;
            continue;
        }
        if ident_call_at(sql, i, name) {
            let open = sql[i + name.len()..]
                .char_indices()
                .find(|(_, c)| !c.is_whitespace())
                .map(|(off, _)| i + name.len() + off)
                .unwrap_or(i + name.len());
            if let Some((args, rest)) = split_call_args(&sql[open + 1..]) {
                let rewritten: Vec<String> = args
                    .iter()
                    .map(|a| rewrite_variadic_fn(a, name, pg_name))
                    .collect();
                if rewritten.len() >= 2 {
                    // SQLite scalar min/max: any NULL argument yields NULL.
                    // PostgreSQL LEAST/GREATEST skip NULLs; poison explicitly.
                    let nulls = rewritten
                        .iter()
                        .map(|a| format!("({a}) IS NULL"))
                        .collect::<Vec<_>>()
                        .join(" OR ");
                    out.push_str("CASE WHEN ");
                    out.push_str(&nulls);
                    out.push_str(" THEN NULL ELSE ");
                    out.push_str(pg_name);
                    out.push('(');
                    out.push_str(&rewritten.join(", "));
                    out.push_str(") END");
                } else {
                    out.push_str(&sql[i..=open]);
                    out.push_str(&rewritten.join(", "));
                    out.push(')');
                }
                i = sql.len() - rest.len();
                continue;
            }
        }
        let ch = sql[i..].chars().next().unwrap_or('\0');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Rewrites `round` / `sum` / `avg` onto Postgres wire types (`Float64` / `Int64`).
fn rewrite_round_sum_avg(sql: &str) -> String {
    let sql = rewrite_round(sql);
    let sql = rewrite_sum_or_avg(&sql, "sum", "BIGINT");
    rewrite_sum_or_avg(&sql, "avg", "DOUBLE PRECISION")
}

/// Rewrites `round(x)` / `round(x, n)` onto `DOUBLE PRECISION`.
fn rewrite_round(sql: &str) -> String {
    let mut i = 0;
    let mut out = String::with_capacity(sql.len() + 32);
    while i < sql.len() {
        if let Some(len) = literal_or_comment_len(&sql[i..]) {
            out.push_str(&sql[i..i + len]);
            i += len;
            continue;
        }
        if ident_call_at(sql, i, "round") {
            let open = sql[i + "round".len()..]
                .char_indices()
                .find(|(_, c)| !c.is_whitespace())
                .map(|(off, _)| i + "round".len() + off)
                .unwrap_or(i + "round".len());
            if let Some((args, rest)) = split_call_args(&sql[open + 1..]) {
                let rewritten: Vec<String> = args.iter().map(|a| rewrite_round(a)).collect();
                if rewritten.len() == 1 {
                    out.push_str(&format!(
                        "CAST(round(CAST(({}) AS NUMERIC)) AS DOUBLE PRECISION)",
                        rewritten[0]
                    ));
                    i = sql.len() - rest.len();
                    continue;
                }
                if rewritten.len() == 2 {
                    out.push_str(&format!(
                        "CAST(round(CAST(({}) AS NUMERIC), {}) AS DOUBLE PRECISION)",
                        rewritten[0], rewritten[1]
                    ));
                    i = sql.len() - rest.len();
                    continue;
                }
            }
        }
        let ch = sql[i..].chars().next().unwrap_or('\0');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Rewrites `sum`/`avg` onto `BIGINT` / `DOUBLE PRECISION`.
fn rewrite_sum_or_avg(sql: &str, name: &str, pg_type: &str) -> String {
    let mut i = 0;
    let mut out = String::with_capacity(sql.len() + 24);
    while i < sql.len() {
        if let Some(len) = literal_or_comment_len(&sql[i..]) {
            out.push_str(&sql[i..i + len]);
            i += len;
            continue;
        }
        if ident_call_at(sql, i, name) {
            let open = sql[i + name.len()..]
                .char_indices()
                .find(|(_, c)| !c.is_whitespace())
                .map(|(off, _)| i + name.len() + off)
                .unwrap_or(i + name.len());
            if let Some((args, rest)) = split_call_args(&sql[open + 1..]) {
                let rewritten: Vec<String> = args
                    .iter()
                    .map(|a| rewrite_sum_or_avg(a, name, pg_type))
                    .collect();
                if rewritten.len() == 1 {
                    out.push_str(&format!("CAST({name}({}) AS {pg_type})", rewritten[0]));
                    i = sql.len() - rest.len();
                    continue;
                }
            }
        }
        let ch = sql[i..].chars().next().unwrap_or('\0');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Rewrites `json_valid(expr) = 0/1` and bare `json_valid(expr)` to `IS [NOT] JSON`.
fn rewrite_json_valid(sql: &str) -> String {
    let mut i = 0;
    let mut out = String::with_capacity(sql.len());
    let name = "json_valid";
    while i < sql.len() {
        if let Some(len) = literal_or_comment_len(&sql[i..]) {
            out.push_str(&sql[i..i + len]);
            i += len;
            continue;
        }
        if ident_call_at(sql, i, name) {
            let open = sql[i + name.len()..]
                .char_indices()
                .find(|(_, c)| !c.is_whitespace())
                .map(|(off, _)| i + name.len() + off)
                .unwrap_or(i + name.len());
            if let Some((args, rest)) = split_call_args(&sql[open + 1..]) {
                if args.len() == 1 {
                    let expr = rewrite_json_valid(&args[0]);
                    let rest_trim = rest.trim_start();
                    if let Some(rest2) = rest_trim.strip_prefix('=') {
                        let rest2 = rest2.trim_start();
                        if let Some(rest2) = strip_json_valid_flag(rest2, '0') {
                            out.push_str(&format!("({expr} IS NOT JSON)"));
                            i = sql.len() - rest2.len();
                            continue;
                        }
                        if let Some(rest2) = strip_json_valid_flag(rest2, '1') {
                            out.push_str(&format!("({expr} IS JSON)"));
                            i = sql.len() - rest2.len();
                            continue;
                        }
                    }
                    out.push_str(&format!("(CASE WHEN ({expr}) IS JSON THEN 1 ELSE 0 END)"));
                    i = sql.len() - rest.len();
                    continue;
                }
            }
        }
        let ch = sql[i..].chars().next().unwrap_or('\0');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// True when `sql[i..]` is a call to `name(` (case-insensitive, word-bounded).
fn ident_call_at(sql: &str, i: usize, name: &str) -> bool {
    let Some(end) = i.checked_add(name.len()).filter(|e| *e <= sql.len()) else {
        return false;
    };
    let Some(prefix) = sql.get(i..end) else {
        return false;
    };
    if !prefix.eq_ignore_ascii_case(name) {
        return false;
    }
    if !word_boundary(sql, i.checked_sub(1)) {
        return false;
    }
    sql.get(end..)
        .and_then(|rest| rest.chars().find(|c| !c.is_whitespace()))
        == Some('(')
}

/// Splits the argument list of a call whose `s` starts just after `(`.
///
/// Returns `(args, rest_after_closing_paren)`.
fn split_call_args(s: &str) -> Option<(Vec<String>, &str)> {
    let mut args = Vec::new();
    let mut start = 0;
    let mut depth = 0i32;
    let mut i = 0;
    while i < s.len() {
        if let Some(len) = literal_or_comment_len(&s[i..]) {
            i += len;
            continue;
        }
        match s[i..].chars().next()? {
            '(' => depth += 1,
            ')' if depth == 0 => {
                let last = s[start..i].trim();
                if !last.is_empty() || !args.is_empty() {
                    args.push(last.to_string());
                }
                return Some((args, &s[i + 1..]));
            }
            ')' => depth -= 1,
            ',' if depth == 0 => {
                args.push(s[start..i].trim().to_string());
                start = i + 1;
            }
            _ => {}
        }
        i += s[i..].chars().next()?.len_utf8();
    }
    None
}

/// Rewrites `json_extract(expr, '$.a.b')` to a guarded `jsonb #>>` extract.
fn rewrite_json_extract(sql: &str) -> String {
    let mut i = 0;
    let mut out = String::with_capacity(sql.len());
    while i < sql.len() {
        if let Some(len) = literal_or_comment_len(&sql[i..]) {
            out.push_str(&sql[i..i + len]);
            i += len;
            continue;
        }
        if ident_call_at(sql, i, "json_extract") {
            let after = &sql[i + "json_extract".len()..];
            let after = after.trim_start();
            let after = after.strip_prefix('(').unwrap_or(after);
            if let Some((expr, json_path, rest2)) = parse_json_extract_args(after) {
                let expr = rewrite_json_extract(expr);
                let pg_path = json_path.replace('.', ",");
                out.push_str(&format!(
                    "(CASE WHEN ({expr}) IS JSON THEN (({expr})::jsonb #>> '{{{pg_path}}}') END)"
                ));
                i = sql.len() - rest2.len();
                continue;
            }
        }
        let ch = sql[i..].chars().next().unwrap_or('\0');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Parses `expr, '$.path')` from `json_extract(` arguments (string-aware).
///
/// The path may already be wrapped as `('$.path' COLLATE "C")` when TEXT
/// collation ran before this rewrite.
fn parse_json_extract_args(after: &str) -> Option<(&str, &str, &str)> {
    let comma = find_top_level_comma(after)?;
    let expr = after[..comma].trim();
    let after_comma = after[comma + 1..].trim_start();
    let (json_path, remainder) = take_json_extract_path_arg(after_comma)?;
    let rest2 = remainder.trim_start().strip_prefix(')')?;
    Some((expr, json_path, rest2))
}

/// `'$.a.b'` or `('$.a.b' COLLATE "C")`, returning the path without `$.`.
fn take_json_extract_path_arg(s: &str) -> Option<(&str, &str)> {
    let s = s.trim_start();
    if let Some(rest) = s.strip_prefix("'$.") {
        let endq = rest.find('\'')?;
        return Some((&rest[..endq], &rest[endq + 1..]));
    }
    let inner = s.strip_prefix('(')?.trim_start().strip_prefix("'$.")?;
    let endq = inner.find('\'')?;
    let path = &inner[..endq];
    let after = inner[endq + 1..].trim_start();
    let after = strip_prefix_ci(after, "COLLATE")?.trim_start();
    let after = after.strip_prefix("\"C\"")?.trim_start();
    let after = after.strip_prefix(')')?;
    Some((path, after))
}

/// Index of the first comma at parenthesis depth 0, skipping literals.
fn find_top_level_comma(s: &str) -> Option<usize> {
    let mut depth = 0i32;
    let mut i = 0;
    while i < s.len() {
        if let Some(len) = literal_or_comment_len(&s[i..]) {
            i += len;
            continue;
        }
        match s[i..].chars().next()? {
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth == 0 => return Some(i),
            ch => {
                i += ch.len_utf8();
                continue;
            }
        }
        i += 1;
    }
    None
}

/// Rewrites the sqlite `julianday` dispatch-latency expression to `EXTRACT(EPOCH …)`.
fn rewrite_julianday_delta(sql: &str) -> String {
    const NEEDLE: &str = "CAST((julianday(?) - julianday((SELECT created_at FROM domain_events WHERE id = ?))) * 86400000 AS INTEGER)";
    const REPL: &str = "CAST(EXTRACT(EPOCH FROM (?::timestamptz - (SELECT created_at::timestamptz FROM domain_events WHERE id = ?))) * 1000 AS BIGINT)";
    replace_in_code(sql, NEEDLE, REPL)
}

/// Mechanical SQLite→Postgres type/identity rewrites for one DDL statement.
///
/// Applied at the adapter execution edge ([`crate::schema_sql_for_backend`],
/// [`crate::lower_binding_ddl_execute_request`]), not by
/// [`lower_canonical_sql`]:
///
/// - `INTEGER PRIMARY KEY AUTOINCREMENT` → `BIGINT PRIMARY KEY` plus an
///   adapter-private transactional identity trigger (not `BIGSERIAL`)
/// - `INTEGER` → `BIGINT` (shared SeaORM entities use `i64` everywhere)
/// - `REAL` → `DOUBLE PRECISION`
/// - `BLOB` → `BYTEA`
/// - `BOOLEAN` is already a Postgres type and is left unchanged
///
/// Token matching is case-insensitive in **type position** of a
/// `CREATE TABLE` column list and `ALTER TABLE … ADD COLUMN` (column names
/// such as `blob` are preserved). Trivia between
/// `INTEGER PRIMARY KEY AUTOINCREMENT` words is skipped.
/// String literals and comments are copied verbatim.
pub(crate) fn rewrite_canonical_ddl_types_for_postgres(sql: &str) -> String {
    let Some(open) = create_table_column_list_open(sql) else {
        return rewrite_postgres_blob_hex_defaults(&rewrite_alter_add_column_types(sql));
    };
    let mut out = String::with_capacity(sql.len() + 16);
    out.push_str(&sql[..=open]);
    let mut i = open + 1;
    let mut depth = 1i32;
    let mut at_column_start = true;
    while i < sql.len() && depth > 0 {
        if let Some(len) = literal_or_comment_len(&sql[i..]) {
            out.push_str(&sql[i..i + len]);
            i += len;
            continue;
        }
        let ch = sql[i..].chars().next().unwrap_or('\0');
        if ch.is_whitespace() {
            out.push(ch);
            i += ch.len_utf8();
            continue;
        }
        if ch == '(' {
            depth += 1;
            out.push('(');
            i += 1;
            continue;
        }
        if ch == ')' {
            depth -= 1;
            out.push(')');
            i += 1;
            at_column_start = false;
            continue;
        }
        if ch == ',' && depth == 1 {
            out.push(',');
            i += 1;
            at_column_start = true;
            continue;
        }
        if depth != 1 || !at_column_start {
            out.push(ch);
            i += ch.len_utf8();
            continue;
        }
        if ident_eq_ci(sql, i, "CONSTRAINT")
            || ident_eq_ci(sql, i, "PRIMARY")
            || ident_eq_ci(sql, i, "UNIQUE")
            || ident_eq_ci(sql, i, "CHECK")
            || ident_eq_ci(sql, i, "FOREIGN")
        {
            at_column_start = false;
            out.push(ch);
            i += ch.len_utf8();
            continue;
        }
        let Some((name_start, name_end)) = ident_span_at(sql, i) else {
            out.push(ch);
            i += ch.len_utf8();
            continue;
        };
        out.push_str(&sql[name_start..name_end]);
        i = name_end;
        let type_at = skip_trivia_idx(sql, i);
        out.push_str(&sql[i..type_at]);
        i = type_at;
        if let Some((end, repl)) = ddl_type_rewrite_at(sql, i) {
            out.push_str(repl);
            i = end;
        } else if let Some((ty_start, ty_end)) = ident_span_at(sql, i) {
            out.push_str(&sql[ty_start..ty_end]);
            i = ty_end;
        }
        at_column_start = false;
    }
    if i < sql.len() {
        out.push_str(&sql[i..]);
    }
    rewrite_postgres_blob_hex_defaults(&out)
}

/// Rewrites `ALTER TABLE … ADD [COLUMN] name TYPE` in type position only.
fn rewrite_alter_add_column_types(sql: &str) -> String {
    let mut i = skip_trivia_idx(sql, 0);
    if !ident_eq_ci(sql, i, "ALTER") {
        return sql.to_string();
    }
    i = skip_trivia_idx(sql, i + "ALTER".len());
    if !ident_eq_ci(sql, i, "TABLE") {
        return sql.to_string();
    }
    i = skip_trivia_idx(sql, i + "TABLE".len());
    if ident_eq_ci(sql, i, "IF") {
        i = skip_trivia_idx(sql, i + "IF".len());
        if ident_eq_ci(sql, i, "EXISTS") {
            i = skip_trivia_idx(sql, i + "EXISTS".len());
        }
    }
    let Some((_, name_end)) = ident_span_at(sql, i) else {
        return sql.to_string();
    };
    i = skip_trivia_idx(sql, name_end);
    if !ident_eq_ci(sql, i, "ADD") {
        return sql.to_string();
    }
    i = skip_trivia_idx(sql, i + "ADD".len());
    if ident_eq_ci(sql, i, "COLUMN") {
        i = skip_trivia_idx(sql, i + "COLUMN".len());
    }
    let Some((_, col_end)) = ident_span_at(sql, i) else {
        return sql.to_string();
    };
    i = skip_trivia_idx(sql, col_end);
    if let Some((end, repl)) = ddl_type_rewrite_at(sql, i) {
        let mut out = String::with_capacity(sql.len() + repl.len());
        out.push_str(&sql[..i]);
        out.push_str(repl);
        out.push_str(&sql[end..]);
        return out;
    }
    sql.to_string()
}

/// Byte offset of the `CREATE TABLE … (` column-list open paren, if present.
fn create_table_column_list_open(sql: &str) -> Option<usize> {
    let mut i = 0;
    while i < sql.len() {
        if let Some(len) = literal_or_comment_len(&sql[i..]) {
            i += len;
            continue;
        }
        if ident_eq_ci(sql, i, "CREATE") {
            let mut j = skip_trivia_idx(sql, i + "CREATE".len());
            if ident_eq_ci(sql, j, "TEMP") {
                j = skip_trivia_idx(sql, j + "TEMP".len());
            } else if ident_eq_ci(sql, j, "TEMPORARY") {
                j = skip_trivia_idx(sql, j + "TEMPORARY".len());
            }
            if !ident_eq_ci(sql, j, "TABLE") {
                return None;
            }
            j = skip_trivia_idx(sql, j + "TABLE".len());
            if ident_eq_ci(sql, j, "IF") {
                j = skip_trivia_idx(sql, j + "IF".len());
                if ident_eq_ci(sql, j, "NOT") {
                    j = skip_trivia_idx(sql, j + "NOT".len());
                }
                if ident_eq_ci(sql, j, "EXISTS") {
                    j = skip_trivia_idx(sql, j + "EXISTS".len());
                }
            }
            let (_, name_end) = ident_span_at(sql, j)?;
            j = skip_trivia_idx(sql, name_end);
            return (sql.as_bytes().get(j) == Some(&b'(')).then_some(j);
        }
        let ch = sql[i..].chars().next()?;
        i += ch.len_utf8();
    }
    None
}

/// Quoted or unquoted identifier span starting at `i`.
fn ident_span_at(sql: &str, i: usize) -> Option<(usize, usize)> {
    let bytes = sql.as_bytes();
    let b = *bytes.get(i)?;
    if b == b'"' {
        return quoted_len(&sql[i..]).map(|len| (i, i + len));
    }
    if b.is_ascii_alphabetic() || b == b'_' {
        let mut j = i + 1;
        while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
            j += 1;
        }
        return Some((i, j));
    }
    None
}

/// Canonical `DEFAULT X'hex'` → Postgres `DEFAULT decode('hex', 'hex')` after BYTEA.
fn rewrite_postgres_blob_hex_defaults(sql: &str) -> String {
    let mut i = 0;
    let mut out = String::with_capacity(sql.len() + 16);
    while i < sql.len() {
        if let Some(len) = literal_or_comment_len(&sql[i..]) {
            out.push_str(&sql[i..i + len]);
            i += len;
            continue;
        }
        if ident_eq_ci(sql, i, "DEFAULT") {
            out.push_str(&sql[i..i + 7]);
            i += 7;
            let j = skip_trivia_idx(sql, i);
            out.push_str(&sql[i..j]);
            i = j;
            if let Some((hex, end)) = take_blob_hex_at(sql, i) {
                out.push_str("decode('");
                out.push_str(&hex);
                out.push_str("', 'hex')");
                i = end;
                continue;
            }
            continue;
        }
        let ch = sql[i..].chars().next().unwrap_or('\0');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Canonical `X'hex'` / `x'hex'` starting at `i`.
fn take_blob_hex_at(sql: &str, i: usize) -> Option<(String, usize)> {
    let rest = sql.get(i..)?;
    if rest.len() < 3 {
        return None;
    }
    let b = rest.as_bytes();
    if !b[0].eq_ignore_ascii_case(&b'x') || b[1] != b'\'' {
        return None;
    }
    let mut j = i + 2;
    while j < sql.len() {
        let c = sql.as_bytes()[j];
        if c == b'\'' {
            return Some((sql[i + 2..j].to_ascii_lowercase(), j + 1));
        }
        if !c.is_ascii_hexdigit() {
            return None;
        }
        j += 1;
    }
    None
}

/// Type/identity rewrite starting at `i`, if `i` is a canonical DDL type token.
fn ddl_type_rewrite_at(sql: &str, i: usize) -> Option<(usize, &'static str)> {
    if ident_eq_ci(sql, i, "INTEGER") {
        let mut j = skip_trivia_idx(sql, i + "INTEGER".len());
        if ident_eq_ci(sql, j, "PRIMARY") {
            j = skip_trivia_idx(sql, j + "PRIMARY".len());
            if ident_eq_ci(sql, j, "KEY") {
                j = skip_trivia_idx(sql, j + "KEY".len());
                if ident_eq_ci(sql, j, "AUTOINCREMENT") {
                    return Some((j + "AUTOINCREMENT".len(), "BIGINT PRIMARY KEY"));
                }
            }
        }
        return Some((i + "INTEGER".len(), "BIGINT"));
    }
    if ident_eq_ci(sql, i, "REAL") {
        return Some((i + "REAL".len(), "DOUBLE PRECISION"));
    }
    if ident_eq_ci(sql, i, "BLOB") {
        return Some((i + "BLOB".len(), "BYTEA"));
    }
    None
}

/// Lowers one canonical Bookclerk **DDL** statement onto PostgreSQL.
///
/// Type/identity rewrites plus [`lower_canonical_to_postgres`] (placeholders,
/// `INSERT OR IGNORE`). Host schema packs use this through
/// [`crate::schema_sql_for_backend`]; binding DDL uses the type rewrite at
/// the adapter edge and then the DML helper pass.
#[must_use]
pub fn lower_canonical_ddl_to_postgres(sql: &str) -> String {
    lower_canonical_to_postgres(&rewrite_canonical_ddl_types_for_postgres(sql))
}

/// True when `s` starts with JSON-valid flag `flag` (`0` or `1`) at a word boundary.
fn strip_json_valid_flag(s: &str, flag: char) -> Option<&str> {
    let rest = s.strip_prefix(flag)?;
    word_boundary(s, Some(flag.len_utf8())).then_some(rest)
}

/// True when the byte at `idx` (or the string edge) is not an identifier char.
fn word_boundary(sql: &str, idx: Option<usize>) -> bool {
    match idx.and_then(|i| sql.as_bytes().get(i)) {
        Some(b) => !(b.is_ascii_alphanumeric() || *b == b'_'),
        None => true,
    }
}

/// True when the ident at `i` equals `word` ignoring ASCII case, with boundaries.
fn ident_eq_ci(sql: &str, i: usize, word: &str) -> bool {
    let Some(end) = i.checked_add(word.len()).filter(|e| *e <= sql.len()) else {
        return false;
    };
    sql.get(i..end)
        .is_some_and(|s| s.eq_ignore_ascii_case(word))
        && word_boundary(sql, i.checked_sub(1))
        && word_boundary(sql, Some(end))
}

/// Byte offset after leading whitespace and comments starting at `i`.
fn skip_trivia_idx(sql: &str, mut i: usize) -> usize {
    if i > sql.len() {
        return sql.len();
    }
    while i < sql.len() && !sql.is_char_boundary(i) {
        i += 1;
    }
    while i < sql.len() {
        let Some(rest) = sql.get(i..) else {
            break;
        };
        let Some(ch) = rest.chars().next() else {
            break;
        };
        if ch.is_whitespace() {
            i += ch.len_utf8();
            continue;
        }
        if let Some(len) = comment_len(rest) {
            i += len;
            continue;
        }
        break;
    }
    i
}

/// Byte offset of keyword `word` in a code span (ASCII case-insensitive).
fn find_word_in_code_ci(sql: &str, word: &str) -> Option<usize> {
    if word.is_empty() {
        return None;
    }
    let mut i = 0;
    while i < sql.len() {
        if let Some(len) = literal_or_comment_len(&sql[i..]) {
            i += len;
            continue;
        }
        if ident_eq_ci(sql, i, word) {
            return Some(i);
        }
        let ch = sql[i..].chars().next()?;
        i += ch.len_utf8();
    }
    None
}

/// Replaces `from` with `to` only in SQL code spans (not strings/comments).
fn replace_in_code(sql: &str, from: &str, to: &str) -> String {
    if from.is_empty() {
        return sql.to_string();
    }
    let mut i = 0;
    let mut out = String::with_capacity(sql.len());
    while i < sql.len() {
        if let Some(len) = literal_or_comment_len(&sql[i..]) {
            out.push_str(&sql[i..i + len]);
            i += len;
            continue;
        }
        if sql[i..].starts_with(from) {
            out.push_str(to);
            i += from.len();
            continue;
        }
        let ch = sql[i..].chars().next().unwrap_or('\0');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Byte offset of the last `needle` in a code span (literals/comments skipped).
pub(crate) fn find_last_in_code(sql: &str, needle: &str) -> Option<usize> {
    find_in_code_from(sql, needle, true)
}

/// Byte offset of `needle` in a code span; `last` keeps scanning for the final match.
fn find_in_code_from(sql: &str, needle: &str, last: bool) -> Option<usize> {
    if needle.is_empty() {
        return None;
    }
    let mut i = 0;
    let mut found = None;
    while i < sql.len() {
        if let Some(len) = literal_or_comment_len(&sql[i..]) {
            i += len;
            continue;
        }
        if sql[i..].starts_with(needle) {
            if !last {
                return Some(i);
            }
            found = Some(i);
            i += needle.len();
            continue;
        }
        let ch = sql[i..].chars().next()?;
        i += ch.len_utf8();
    }
    found
}

/// Leading comments and whitespace.
fn skip_trivia(sql: &str) -> &str {
    let mut i = 0;
    while i < sql.len() {
        let rest = &sql[i..];
        let Some(ch) = rest.chars().next() else {
            break;
        };
        if ch.is_whitespace() {
            i += ch.len_utf8();
            continue;
        }
        if let Some(len) = comment_len(rest) {
            i += len;
            continue;
        }
        break;
    }
    &sql[i..]
}

/// ASCII-case-insensitive prefix strip.
fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    if s.len() < prefix.len() {
        return None;
    }
    if s.get(..prefix.len())?.eq_ignore_ascii_case(prefix) {
        Some(&s[prefix.len()..])
    } else {
        None
    }
}

/// Length of a string, identifier, dollar-quote, or comment at `s`, if any.
fn literal_or_comment_len(s: &str) -> Option<usize> {
    comment_len(s).or_else(|| quoted_len(s))
}

/// Length of a `--` line comment or `/* */` block at `s`.
fn comment_len(s: &str) -> Option<usize> {
    if s.starts_with("--") {
        let nl = s.find('\n').unwrap_or(s.len());
        return Some(nl);
    }
    if s.starts_with("/*") {
        return s.find("*/").map(|i| i + 2).or(Some(s.len()));
    }
    None
}

/// Length of a quoted identifier/string or dollar-quote at `s`.
fn quoted_len(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    if bytes.is_empty() {
        return None;
    }
    match bytes[0] {
        b'\'' => scan_quote(s, '\''),
        b'"' => scan_quote(s, '"'),
        b'$' => dollar_quote_len(s),
        _ => None,
    }
}

/// Length of a `'…'` or `"…"` literal, honoring doubled escapes.
fn scan_quote(s: &str, q: char) -> Option<usize> {
    let mut chars = s.char_indices();
    chars.next()?;
    while let Some((i, ch)) = chars.next() {
        if ch == q {
            if s[i + q.len_utf8()..].starts_with(q) {
                chars.next();
                continue;
            }
            return Some(i + q.len_utf8());
        }
    }
    Some(s.len())
}

/// Length of a PostgreSQL `$tag$…$tag$` dollar quote at `s`.
fn dollar_quote_len(s: &str) -> Option<usize> {
    let rest = s.get(1..)?;
    let tag_end = rest.find('$').filter(|i| {
        rest[..*i]
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
    })?;
    let tag = &s[..=tag_end + 1];
    let after = s.get(tag.len()..)?;
    let close = after.find(tag)?;
    Some(tag.len() + close + tag.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookclerk_plugin_abi::{
        sql_type_env_from_canonical_ddl, sql_type_env_from_canonical_statements,
        typecheck_execute_request_proofs, DbPlanStatementKind, DbResultSelection, DbValue,
        ExecuteRequest, SqlType, SqlTypeEnv, TypedDbStatement,
    };

    fn proof_of(sql: &str, env: &SqlTypeEnv) -> ResolvedStatement {
        let req = ExecuteRequest {
            operation_id: "t".into(),
            request_hash: String::new(),
            deadline_unix_ms: 0,
            statements: vec![TypedDbStatement {
                sql: sql.into(),
                parameters: Vec::new(),
                kind: DbPlanStatementKind::Select,
                max_rows: 0,
                result_selection: DbResultSelection::Rows,
            }],
        };
        typecheck_execute_request_proofs(&req, env)
            .unwrap_or_else(|err| panic!("{sql}: {err}"))
            .remove(0)
    }

    fn lower_pg(sql: &str, env: &SqlTypeEnv) -> String {
        let proof = proof_of(sql, env);
        lower_canonical_sql_typed(DatabaseBackend::Postgres, sql, Some(&proof))
            .unwrap_or_else(|err| panic!("{sql}: {err}"))
    }

    #[test]
    fn postgres_rewrites_placeholders_and_json_extract() {
        let sql =
            lower_canonical_to_postgres("SELECT json_extract(payload, '$.v') FROM t WHERE id = ?");
        assert_eq!(
            sql,
            "SELECT (CASE WHEN (payload) IS JSON THEN ((payload)::jsonb #>> '{v}') END) FROM t WHERE id = $1"
        );
    }

    #[test]
    fn sqlite_backend_is_identity_except_insert_or_ignore() {
        assert_eq!(
            lower_canonical_sql(DatabaseBackend::Sqlite, "a = ? AND b = ?"),
            "a = ? AND b = ?"
        );
        assert_eq!(
            lower_canonical_sql(
                DatabaseBackend::Sqlite,
                "INSERT OR IGNORE INTO t (id) VALUES (?)"
            ),
            "INSERT INTO t (id) VALUES (?) ON CONFLICT DO NOTHING"
        );
    }

    #[test]
    fn placeholders_inside_strings_and_comments_are_preserved() {
        let sql = lower_canonical_to_postgres(
            "SELECT '?' AS literal, /* ? */ ? FROM t WHERE x = '?' -- ?\nAND y = ?",
        );
        assert!(sql.contains("'?'"), "{sql}");
        assert!(sql.contains("/* ? */"), "{sql}");
        assert!(sql.contains("-- ?"), "{sql}");
        assert!(sql.contains("$1"), "{sql}");
        assert!(sql.contains("$2"), "{sql}");
        assert!(!sql.contains("$3"), "{sql}");
    }

    #[test]
    fn sqlite_family_does_not_json_patch_json_object() {
        let keys: Vec<String> = (0..16).map(|i| format!("'{i}', {i}")).collect();
        let sql = format!("SELECT json_object({})", keys.join(", "));
        let out = lower_canonical_sql(DatabaseBackend::Sqlite, &sql);
        assert_eq!(out, sql, "32-arg json_object must stay identity: {out}");
        assert!(!out.contains("json_patch"), "{out}");
        let with_null = lower_canonical_sql(
            DatabaseBackend::Sqlite,
            "SELECT json_object('a', 1, 'b', NULL, 'a', NULL)",
        );
        assert_eq!(
            with_null,
            "SELECT json_object('a', 1, 'b', NULL, 'a', NULL)"
        );
        assert!(!with_null.contains("json_patch"), "{with_null}");
        let lit = lower_canonical_sql(DatabaseBackend::Sqlite, "SELECT json_object('k;semi', 'v')");
        assert!(lit.contains("'k;semi'"), "{lit}");
        assert!(!lit.contains("json_patch"), "{lit}");
    }

    #[test]
    fn dollar_quotes_are_preserved() {
        let sql = lower_canonical_to_postgres("SELECT $tag$?$tag$, ?");
        assert!(sql.contains("$tag$?$tag$"), "{sql}");
        assert!(sql.contains("$1"), "{sql}");
        assert!(!sql.contains("$2"), "{sql}");
    }

    #[test]
    fn json_extract_inside_a_string_is_not_rewritten() {
        let sql = lower_canonical_to_postgres("SELECT 'json_extract(payload, ''$.v'')', ?");
        assert!(sql.contains("json_extract(payload"), "{sql}");
        assert!(sql.contains("$1"), "{sql}");
    }

    #[test]
    fn nested_json_extract_lowers() {
        let sql = lower_canonical_to_postgres(
            "SELECT json_extract(json_extract(payload, '$.a'), '$.b') FROM t",
        );
        assert!(!sql.contains("json_extract("), "{sql}");
        assert!(sql.contains("#>> '{a}'"), "{sql}");
        assert!(sql.contains("#>> '{b}'"), "{sql}");
    }

    #[test]
    fn postgres_rewrites_variadic_min_max_and_json_valid() {
        assert_eq!(
            lower_canonical_to_postgres("SELECT MAX(attempt_count, 1), min(a, b)"),
            "SELECT CASE WHEN (attempt_count) IS NULL OR (1) IS NULL THEN NULL ELSE GREATEST(attempt_count, 1) END, CASE WHEN (a) IS NULL OR (b) IS NULL THEN NULL ELSE LEAST(a, b) END"
        );
        assert_eq!(
            lower_canonical_to_postgres("SELECT max(x)"),
            "SELECT max(x)"
        );
        let sql = lower_canonical_to_postgres("WHERE json_valid(doc) = 1 AND json_valid(body) = 0");
        assert!(sql.contains("(doc IS JSON)"), "{sql}");
        assert!(sql.contains("(body IS NOT JSON)"), "{sql}");
        assert!(!sql.contains("json_valid"), "{sql}");
        let bare = lower_canonical_to_postgres("SELECT json_valid(payload) FROM t");
        assert!(
            bare.contains("CASE WHEN (payload) IS JSON THEN 1 ELSE 0 END"),
            "{bare}"
        );
    }

    #[test]
    fn postgres_rewrites_ifnull_and_json_object_case_insensitively() {
        assert_eq!(
            lower_canonical_to_postgres("SELECT ifnull(NULL, 5), IFNULL(x, 0)"),
            "SELECT COALESCE(NULL, 5), COALESCE(x, 0)"
        );
        let sql = lower_canonical_to_postgres("SELECT json_object('k', ?), JSON_OBJECT('a', 1)");
        assert!(sql.contains("(json_build_object('k', $1)::text)"), "{sql}");
        assert!(sql.contains("(json_build_object('a', 1)::text)"), "{sql}");
        assert!(!sql.to_ascii_lowercase().contains("json_object("), "{sql}");
    }

    #[test]
    fn postgres_json_object_is_text_except_nested_constructor_values() {
        let length = lower_canonical_to_postgres("SELECT length(json_object('a', 1, 'b', NULL))");
        assert_eq!(
            length,
            "SELECT length((json_build_object('a', 1, 'b', NULL)::text))"
        );
        let nested = lower_canonical_to_postgres(
            "SELECT json_extract(json_object('n', json_object('k', 'v')), '$.n')",
        );
        assert!(
            nested.contains("json_build_object('n', json_build_object('k', 'v'))"),
            "{nested}"
        );
        assert!(
            !nested.contains("json_build_object('k', 'v')::text"),
            "nested constructor values must stay json: {nested}"
        );
        assert!(nested.contains("::text"), "{nested}");
        let portable = lower_canonical_to_postgres(crate::sql_v1::PORTABLE_JSON_OBJECT_SEMANTICS);
        assert!(
            portable.contains("length((json_build_object('a', 1, 'b', NULL)::text))"),
            "{portable}"
        );
        assert!(
            portable.contains("json_build_object('n', json_build_object('k', 'v'))"),
            "{portable}"
        );
        assert!(
            !portable.to_ascii_lowercase().contains("json_object("),
            "{portable}"
        );
    }

    #[test]
    fn portable_select_lowers_helpers_for_postgres() {
        let sql = lower_canonical_to_postgres(crate::sql_v1::PORTABLE_SELECT);
        assert!(!sql.to_ascii_lowercase().contains("ifnull("), "{sql}");
        assert!(!sql.to_ascii_lowercase().contains("json_object("), "{sql}");
        assert!(sql.contains("COALESCE(NULL, 5)"), "{sql}");
        assert!(sql.contains("LEAST(1, 2)"), "{sql}");
        assert!(sql.contains("GREATEST(3, 1)"), "{sql}");
        assert!(
            sql.contains("CASE WHEN (1) IS NULL OR (2) IS NULL"),
            "{sql}"
        );
        assert!(sql.contains("json_build_object"), "{sql}");
        assert!(!sql.contains("json_extract("), "{sql}");
        assert!(!sql.contains("json_valid("), "{sql}");
    }

    #[test]
    fn postgres_dml_lowering_does_not_rewrite_ddl_types() {
        let canonical =
            "CREATE TABLE IF NOT EXISTS t (id INTEGER PRIMARY KEY AUTOINCREMENT, b BLOB)";
        let sql = lower_canonical_sql(DatabaseBackend::Postgres, canonical);
        assert!(
            sql.contains("AUTOINCREMENT") && sql.contains("BLOB"),
            "DDL type/identity lowering is adapter-owned, not generic DML lowering: {sql}"
        );
        let types = rewrite_canonical_ddl_types_for_postgres(canonical);
        assert!(types.contains("BIGINT PRIMARY KEY"), "{types}");
        assert!(types.contains("BYTEA"), "{types}");
        assert!(!types.contains("AUTOINCREMENT"), "{types}");
        assert!(!types.contains("BLOB"), "{types}");
    }

    #[test]
    fn postgres_ddl_and_returning_lowering_is_case_insensitive() {
        let types = rewrite_canonical_ddl_types_for_postgres(
            "create table t (id integer /*x*/ primary key autoincrement, b blob, r real, flag boolean)",
        );
        assert!(types.contains("BIGINT PRIMARY KEY"), "{types}");
        assert!(types.contains("BYTEA"), "{types}");
        assert!(types.contains("DOUBLE PRECISION"), "{types}");
        assert!(
            types.contains("boolean") && !types.contains("BOOLEAN"),
            "BOOLEAN is already Postgres-native and must not be rewritten: {types}"
        );
        assert!(!types.contains("autoincrement"), "{types}");
        assert!(!types.contains("blob"), "{types}");
        assert!(!types.contains("integer"), "{types}");

        let named_blob = rewrite_canonical_ddl_types_for_postgres(
            "CREATE TABLE t (blob BLOB, payload blob, flag BOOLEAN)",
        );
        assert!(
            named_blob.contains("blob BYTEA") && named_blob.contains("payload BYTEA"),
            "column names must be preserved: {named_blob}"
        );
        assert!(named_blob.contains("BOOLEAN"), "{named_blob}");
        assert!(!named_blob.contains("BYTEA BYTEA"), "{named_blob}");

        let alter = rewrite_canonical_ddl_types_for_postgres(
            "ALTER TABLE jobs ADD COLUMN lease_generation INTEGER NOT NULL DEFAULT 0",
        );
        assert!(
            alter.contains("lease_generation BIGINT") && !alter.contains("INTEGER"),
            "{alter}"
        );
        let alter_lc =
            rewrite_canonical_ddl_types_for_postgres("alter table jobs add column payload blob");
        assert!(
            alter_lc.contains("payload BYTEA") && !alter_lc.contains("blob"),
            "{alter_lc}"
        );

        let mixed = rewrite_canonical_ddl_types_for_postgres(
            "Create Table t (id Integer Primary Key Autoincrement, payload Blob, flag Boolean)",
        );
        assert!(mixed.contains("BIGINT PRIMARY KEY"), "{mixed}");
        assert!(mixed.contains("BYTEA"), "{mixed}");
        assert!(mixed.contains("Boolean"), "{mixed}");

        let sql = rewrite_insert_or_ignore_unique_conflict(
            "insert or ignore into t (id) values (?) returning id",
        );
        assert!(
            sql.contains("ON CONFLICT DO NOTHING RETURNING"),
            "conflict clause must precede RETURNING: {sql}"
        );
        assert!(!sql.contains("ON CONFLICT DO NOTHING returning"), "{sql}");
        let after_conflict = sql.split("ON CONFLICT DO NOTHING").nth(1).unwrap_or("");
        assert!(
            after_conflict.to_ascii_uppercase().contains("RETURNING"),
            "{sql}"
        );
    }

    #[test]
    fn json_valid_flag_does_not_match_multi_digit() {
        let sql = lower_canonical_to_postgres("WHERE json_valid(doc) = 10");
        assert!(
            sql.contains("CASE WHEN (doc) IS JSON THEN 1 ELSE 0 END") && sql.contains("= 10"),
            "{sql}"
        );
        assert!(!sql.contains("doc IS JSON)"), "{sql}");
    }

    #[test]
    fn sql_v1_corpus_drives_lowering() {
        let raw = include_str!("../testdata/sql_v1/corpus.json");
        let corpus: serde_json::Value = serde_json::from_str(raw).expect("sql_v1 corpus JSON");
        assert_eq!(corpus["contractVersion"], 1);
        for entry in corpus["lowering"].as_array().expect("lowering") {
            let id = entry["id"].as_str().unwrap_or("?");
            let canonical = entry["canonical"].as_str().expect("canonical");
            let got = lower_canonical_to_postgres(canonical);
            if let Some(exact) = entry["postgres"].as_str() {
                assert_eq!(got, exact, "corpus {id}");
            }
            if let Some(contains) = entry["postgresContains"].as_array() {
                for needle in contains {
                    let n = needle.as_str().unwrap();
                    assert!(got.contains(n), "corpus {id}: expected `{n}` in:\n{got}");
                }
            }
            if let Some(forbidden) = entry["postgresNotContains"].as_array() {
                for needle in forbidden {
                    let n = needle.as_str().unwrap();
                    assert!(!got.contains(n), "corpus {id}: unexpected `{n}` in:\n{got}");
                }
            }
        }
        for entry in corpus["sqliteIdentity"].as_array().expect("sqliteIdentity") {
            let id = entry["id"].as_str().unwrap_or("?");
            let canonical = entry["canonical"].as_str().expect("canonical");
            assert_eq!(
                lower_canonical_sql(DatabaseBackend::Sqlite, canonical),
                canonical,
                "corpus {id}"
            );
        }
    }

    #[test]
    fn postgres_order_by_preserves_host_desugared_nulls() {
        let canonical =
            bookclerk_plugin_abi::desugar_canonical_sql("SELECT a FROM t ORDER BY a, b DESC");
        assert_eq!(
            canonical,
            "SELECT a FROM t ORDER BY a NULLS FIRST, b DESC NULLS LAST"
        );
        assert_eq!(lower_canonical_to_postgres(&canonical), canonical);
        assert_eq!(
            lower_canonical_to_postgres("SELECT a FROM t ORDER BY a ASC NULLS LAST"),
            "SELECT a FROM t ORDER BY a ASC NULLS LAST"
        );
        // Adapter mechanical lowering does not insert unspecified NULLS.
        assert_eq!(
            lower_canonical_to_postgres("SELECT a FROM t ORDER BY a"),
            "SELECT a FROM t ORDER BY a"
        );
    }

    /// Mirrors backup capture_select: host desugar, then proof-directed Postgres lower.
    fn capture_lower_pg(sql: &str, env: &SqlTypeEnv) -> String {
        let desugared = bookclerk_plugin_abi::desugar_canonical_sql(sql);
        let proof = proof_of(&desugared, env);
        lower_canonical_sql_typed(DatabaseBackend::Postgres, &desugared, Some(&proof))
            .unwrap_or_else(|err| panic!("{desugared}: {err}"))
    }

    #[test]
    fn postgres_backup_catalog_select_collates_text_and_keeps_nulls() {
        let env = sql_type_env_from_canonical_statements([
            bookclerk_plugin_abi::sql_ddl_create_table_sql(),
            bookclerk_plugin_abi::sql_schema_create_table_sql(),
        ]);
        let catalog = format!(
            "SELECT kind, name, table_name, canonical_sql FROM {} \
             ORDER BY kind, name LIMIT 1000 OFFSET 0",
            bookclerk_plugin_abi::SQL_DDL_TABLE
        );
        let catalog_pg = capture_lower_pg(&catalog, &env);
        assert!(catalog_pg.contains("(kind COLLATE \"C\")"), "{catalog_pg}");
        assert!(
            catalog_pg.contains("ORDER BY kind NULLS FIRST, name NULLS FIRST"),
            "{catalog_pg}"
        );

        let mut items = SqlTypeEnv::new();
        items.insert_table(
            "items",
            [("k".into(), SqlType::Text), ("extra".into(), SqlType::Text)],
        );
        let items_pg = capture_lower_pg(
            "SELECT k, extra FROM items ORDER BY k ASC NULLS FIRST, extra ASC NULLS FIRST LIMIT 1000 OFFSET 0",
            &items,
        );
        assert!(items_pg.contains("(k COLLATE \"C\")"), "{items_pg}");
        assert!(
            items_pg.contains("ORDER BY k ASC NULLS FIRST, extra ASC NULLS FIRST"),
            "{items_pg}"
        );
    }

    #[test]
    fn postgres_round_sum_avg_cast_to_wire_types() {
        let sql = lower_canonical_to_postgres("SELECT round(r, 2), sum(n), avg(n) FROM typed");
        assert!(
            sql.contains("CAST(round(CAST((r) AS NUMERIC), 2) AS DOUBLE PRECISION)"),
            "{sql}"
        );
        assert!(sql.contains("CAST(sum(n) AS BIGINT)"), "{sql}");
        assert!(sql.contains("CAST(avg(n) AS DOUBLE PRECISION)"), "{sql}");
    }

    #[test]
    fn insert_or_ignore_select_wraps_source() {
        let sql = rewrite_insert_or_ignore_unique_conflict(
            "INSERT OR IGNORE INTO t (id) SELECT ? RETURNING id",
        );
        assert!(
            sql.contains("SELECT * FROM (SELECT ?) AS bookclerk_src WHERE true"),
            "{sql}"
        );
        assert!(sql.contains("ON CONFLICT DO NOTHING RETURNING"), "{sql}");
        let values = rewrite_insert_or_ignore_unique_conflict(
            "INSERT OR IGNORE INTO t (id) VALUES (?) RETURNING id",
        );
        assert!(
            !values.contains("bookclerk_src"),
            "VALUES must stay unwrapped: {values}"
        );
        let glob = lower_canonical_sql(
            DatabaseBackend::Sqlite,
            "SELECT 1 WHERE 'A' LIKE 'a' AND body LIKE ?",
        );
        assert!(glob.contains("GLOB "), "{glob}");
        assert!(glob.contains("replace("), "{glob}");
        assert!(!glob.contains(" LIKE "), "{glob}");
        let glob_na = lower_canonical_sql(
            DatabaseBackend::Sqlite,
            "SELECT CASE WHEN 'İ' LIKE 'i' THEN 1 ELSE 0 END",
        );
        assert!(glob_na.contains("GLOB "), "{glob_na}");
        assert!(!glob_na.contains(" LIKE "), "{glob_na}");
        let hex = rewrite_canonical_ddl_types_for_postgres(
            "CREATE TABLE t (payload BLOB DEFAULT X'deadbeef')",
        );
        assert!(hex.contains("DEFAULT decode('deadbeef', 'hex')"), "{hex}");
        assert!(hex.contains("BYTEA"), "{hex}");
        assert!(!hex.contains("X'"), "{hex}");
    }

    #[test]
    fn postgres_collate_skips_insert_update_and_conflict_targets() {
        let mut env = SqlTypeEnv::new();
        env.insert_table(
            "t",
            [
                ("body".into(), SqlType::Text),
                ("id".into(), SqlType::Integer),
            ],
        );
        let dml = lower_pg("INSERT OR IGNORE INTO t (id, body) VALUES (1, 'A')", &env);
        assert!(dml.contains("INSERT INTO t (id, body)"), "{dml}");
        assert!(!dml.contains("INSERT INTO t (id, (body"), "{dml}");
        assert!(dml.contains("('A' COLLATE \"C\")"), "{dml}");

        let where_sql = lower_pg("SELECT body FROM t WHERE body = 'A' ORDER BY body", &env);
        assert!(where_sql.contains("(body COLLATE \"C\")"), "{where_sql}");
        assert!(where_sql.contains("('A' COLLATE \"C\")"), "{where_sql}");
        assert!(!where_sql.contains("FROM (t COLLATE"), "{where_sql}");

        let mut mixed = SqlTypeEnv::new();
        mixed.insert_table("a", vec![("name".into(), SqlType::Text)]);
        mixed.insert_table("b", vec![("name".into(), SqlType::Integer)]);
        let host_order = lower_pg("SELECT name FROM a ORDER BY name", &mixed);
        assert!(
            host_order.contains("(name COLLATE \"C\")"),
            "host-authored TEXT ORDER BY must collate: {host_order}"
        );
        let alias = lower_pg("SELECT b.name FROM a AS b ORDER BY b.name", &mixed);
        assert!(
            alias.contains("(b.name COLLATE \"C\")") || alias.contains("COLLATE \"C\""),
            "{alias}"
        );

        let update = lower_pg("UPDATE t SET body = 'A' WHERE body = 'B'", &env);
        assert!(update.contains("SET body ="), "{update}");
        assert!(!update.contains("SET (body COLLATE"), "{update}");
        assert!(update.contains("WHERE (body COLLATE \"C\")"), "{update}");
        assert!(update.contains("('A' COLLATE \"C\")"), "{update}");
    }

    #[test]
    fn div_and_mod_by_zero_are_host_desugars() {
        let sql = bookclerk_plugin_abi::desugar_canonical_sql("SELECT 1 / 0, 4 % 0, a / b");
        assert!(sql.contains("NULLIF(0, 0)"), "{sql}");
        assert!(sql.contains("NULLIF(b, 0)"), "{sql}");
        assert_eq!(
            lower_canonical_sql(DatabaseBackend::Postgres, &sql),
            sql,
            "adapter must not double-wrap NULLIF: {sql}"
        );
        let misleading = bookclerk_plugin_abi::desugar_canonical_sql("SELECT 1 / NULLIF(0, 1)");
        assert_eq!(misleading, "SELECT 1 / NULLIF(NULLIF(0, 1), 0)");
        assert_eq!(
            lower_canonical_sql(DatabaseBackend::Postgres, &misleading),
            misleading,
            "adapter must not wrap an already-structural NULLIF(expr, 0): {misleading}"
        );
        assert_eq!(
            lower_canonical_sql(DatabaseBackend::Sqlite, &misleading),
            misleading
        );
    }

    #[test]
    fn postgres_like_non_ascii_does_not_panic_on_fn_scan() {
        let sql =
            lower_canonical_to_postgres("SELECT CASE WHEN 'İ' LIKE 'i' THEN 1 ELSE 0 END AS c0");
        assert!(sql.contains("LIKE"), "{sql}");
        assert!(sql.contains('İ'), "{sql}");
        let typed = lower_canonical_sql_typed(
            DatabaseBackend::Postgres,
            "SELECT CASE WHEN 'İ' LIKE 'i' THEN 1 ELSE 0 END AS c0",
            None,
        )
        .expect("untyped lowering");
        assert!(typed.contains("LIKE"), "{typed}");
    }

    #[test]
    fn div_operand_covers_call_qualified_unary_and_cast() {
        let sql = bookclerk_plugin_abi::desugar_canonical_sql(
            "SELECT 10 / abs(n), 10 / t.n, 10 / -n, 10 / CAST(n AS INTEGER), 10 / (n + 1)",
        );
        assert!(sql.contains("NULLIF(abs(n), 0)"), "{sql}");
        assert!(sql.contains("NULLIF(t.n, 0)"), "{sql}");
        assert!(sql.contains("NULLIF(-n, 0)"), "{sql}");
        assert!(sql.contains("NULLIF(CAST(n AS INTEGER), 0)"), "{sql}");
        assert!(sql.contains("NULLIF((n + 1), 0)"), "{sql}");
    }

    #[test]
    fn typed_lowering_fails_closed_on_mismatched_proof() {
        let env = SqlTypeEnv::new();
        let proof = proof_of("SELECT 1", &env);
        let err = lower_canonical_sql_typed(DatabaseBackend::Postgres, "SELECT 2", Some(&proof))
            .expect_err("mismatched hash");
        assert!(err.to_string().contains("proof"), "{err}");
    }

    #[test]
    fn collate_wraps_literals_that_look_like_bind_or_collate_text() {
        let sql = lower_pg("SELECT '$ä', '{ä', 'COLLATEä'", &SqlTypeEnv::new());
        assert!(sql.contains("('$ä' COLLATE \"C\")"), "{sql}");
        assert!(sql.contains("('{ä' COLLATE \"C\")"), "{sql}");
        assert!(sql.contains("('COLLATEä' COLLATE \"C\")"), "{sql}");
    }

    #[test]
    fn typed_json_extract_survives_text_collate_on_document() {
        let env = sql_type_env_from_canonical_ddl("CREATE TABLE t (payload TEXT)");
        let sql = lower_pg("SELECT json_extract(payload, '$.k') FROM t", &env);
        assert!(!sql.to_ascii_lowercase().contains("json_extract("), "{sql}");
        assert!(sql.contains("#>>"), "{sql}");
        assert!(sql.contains("COLLATE \"C\""), "{sql}");
    }

    #[test]
    fn postgres_collate_comes_from_text_collate_sites() {
        let env = sql_type_env_from_canonical_ddl("CREATE TABLE t (name TEXT)");
        let sql = "SELECT name FROM t WHERE name = 'a'";
        let proof = proof_of(sql, &env);
        assert!(
            !proof.text_collate_sites.is_empty(),
            "TEXT comparisons must record collate sites"
        );
        let lowered = lower_canonical_sql_typed(DatabaseBackend::Postgres, sql, Some(&proof))
            .expect("postgres collate from sites");
        assert!(
            lowered.contains("COLLATE \"C\""),
            "postgres must wrap TEXT from proof sites: {lowered}"
        );
        let mut without_sites = proof.clone();
        without_sites.text_collate_sites.clear();
        let no_collate =
            lower_canonical_sql_typed(DatabaseBackend::Postgres, sql, Some(&without_sites))
                .expect("empty sites skip collate");
        assert!(
            !no_collate.contains("COLLATE"),
            "clearing text_collate_sites must skip COLLATE: {no_collate}"
        );
    }

    #[test]
    fn integer_overflow_lowers_to_null_case() {
        let sql = lower_pg(
            "SELECT 9223372036854775807 + 1, abs(-9223372036854775807 - 1)",
            &SqlTypeEnv::new(),
        );
        assert!(sql.contains("THEN NULL"), "{sql}");
        assert!(sql.contains("abs("), "{sql}");
        let sqlite = {
            let proof = proof_of("SELECT 9223372036854775807 + 1", &SqlTypeEnv::new());
            lower_canonical_sql_typed(
                DatabaseBackend::Sqlite,
                "SELECT 9223372036854775807 + 1",
                Some(&proof),
            )
            .expect("sqlite overflow wrap")
        };
        assert!(sqlite.contains("THEN NULL"), "{sqlite}");
    }

    #[test]
    fn integer_overflow_wrap_keeps_each_placeholder_once() {
        let sql = "SELECT ? + ?";
        let req = ExecuteRequest {
            operation_id: "t".into(),
            request_hash: String::new(),
            deadline_unix_ms: 0,
            statements: vec![TypedDbStatement {
                sql: sql.into(),
                parameters: vec![DbValue::Int64(1), DbValue::Int64(2)],
                kind: DbPlanStatementKind::Select,
                max_rows: 0,
                result_selection: DbResultSelection::Rows,
            }],
        };
        let proof = typecheck_execute_request_proofs(&req, &SqlTypeEnv::new())
            .expect("typecheck")
            .remove(0);
        let sqlite = lower_canonical_sql_typed(DatabaseBackend::Sqlite, sql, Some(&proof))
            .expect("sqlite overflow wrap");
        assert_eq!(sqlite.bytes().filter(|b| *b == b'?').count(), 2, "{sqlite}");
        assert!(sqlite.contains("_bc_ov"), "{sqlite}");
        assert!(!sqlite.contains("LATERAL"), "{sqlite}");
        let pg = lower_canonical_sql_typed(DatabaseBackend::Postgres, sql, Some(&proof))
            .expect("postgres overflow wrap");
        assert!(pg.contains("$1") && pg.contains("$2"), "{pg}");
        assert!(!pg.contains("$3"), "{pg}");
        assert!(pg.contains("LATERAL"), "{pg}");
    }

    #[test]
    fn integer_overflow_min_is_cast_not_bigint_subtraction() {
        let sql = lower_pg("SELECT abs(-3)", &SqlTypeEnv::new());
        assert!(
            sql.contains("CAST('-9223372036854775808' AS BIGINT)"),
            "{sql}"
        );
        assert!(!sql.contains("9223372036854775807 - 1"), "{sql}");
        let sqlite = {
            let proof = proof_of("SELECT abs(-3)", &SqlTypeEnv::new());
            lower_canonical_sql_typed(DatabaseBackend::Sqlite, "SELECT abs(-3)", Some(&proof))
                .expect("sqlite abs wrap")
        };
        assert!(
            sqlite.contains("CAST('-9223372036854775808' AS INTEGER)"),
            "{sqlite}"
        );
        assert!(!sqlite.contains("9223372036854775807 - 1"), "{sqlite}");
    }

    #[test]
    fn typed_lowering_binds_proof_to_canonical_sql_before_limit_wrap() {
        let sql = "SELECT 1";
        let proof = proof_of(sql, &SqlTypeEnv::new());
        let capped = crate::cap_query_sql(sql, 5);
        let err = lower_canonical_sql_typed(DatabaseBackend::Sqlite, &capped, Some(&proof))
            .expect_err("capped SQL is not the proof's canonical text");
        assert!(err.to_string().contains("proof"), "{err}");
        lower_canonical_sql_typed(DatabaseBackend::Sqlite, sql, Some(&proof))
            .expect("canonical SQL matches the proof");
    }

    #[test]
    fn nested_integer_arith_lowers_innermost_first() {
        let env = sql_type_env_from_canonical_ddl("CREATE TABLE t (n INTEGER, body TEXT)");
        let sql = "SELECT 1 + abs(n) FROM t";
        let sqlite = {
            let proof = proof_of(sql, &env);
            lower_canonical_sql_typed(DatabaseBackend::Sqlite, sql, Some(&proof))
                .expect("nested abs")
        };
        assert!(sqlite.contains("abs(a)"), "{sqlite}");
        assert!(
            sqlite.matches("_bc_ov").count() >= 2,
            "inner abs and outer add each need a derived table: {sqlite}"
        );
        let mul = "SELECT 1 + 2 * 3";
        let sqlite_mul = {
            let proof = proof_of(mul, &SqlTypeEnv::new());
            lower_canonical_sql_typed(DatabaseBackend::Sqlite, mul, Some(&proof))
                .expect("nested mul")
        };
        assert!(sqlite_mul.matches("_bc_ov").count() >= 2, "{sqlite_mul}");
        let binds = "SELECT ? + abs(?)";
        let req = ExecuteRequest {
            operation_id: "t".into(),
            request_hash: String::new(),
            deadline_unix_ms: 0,
            statements: vec![TypedDbStatement {
                sql: binds.into(),
                parameters: vec![DbValue::Int64(1), DbValue::Int64(-2)],
                kind: DbPlanStatementKind::Select,
                max_rows: 0,
                result_selection: DbResultSelection::Rows,
            }],
        };
        let proof = typecheck_execute_request_proofs(&req, &SqlTypeEnv::new())
            .expect("typecheck")
            .remove(0);
        let sqlite_b = lower_canonical_sql_typed(DatabaseBackend::Sqlite, binds, Some(&proof))
            .expect("bind nested");
        assert_eq!(
            sqlite_b.bytes().filter(|b| *b == b'?').count(),
            2,
            "{sqlite_b}"
        );
        let pg_b = lower_canonical_sql_typed(DatabaseBackend::Postgres, binds, Some(&proof))
            .expect("pg bind nested");
        assert!(pg_b.contains("$1") && pg_b.contains("$2"), "{pg_b}");
        assert!(!pg_b.contains("$3"), "{pg_b}");
    }

    #[test]
    fn text_collate_inside_overflow_is_remapped() {
        let env = sql_type_env_from_canonical_ddl("CREATE TABLE t (body TEXT)");
        let sql = "SELECT length(body) + 1 FROM t";
        let pg = lower_pg(sql, &env);
        assert!(pg.contains("THEN NULL"), "{pg}");
        assert!(
            pg.contains("length((body COLLATE \"C\"))") || pg.contains("(body COLLATE \"C\")"),
            "TEXT span inside overflow must already be collated: {pg}"
        );
        assert!(pg.contains("LATERAL"), "{pg}");
    }

    #[test]
    fn json_path_like_literal_outside_extract_still_collates() {
        let sql = "SELECT CASE WHEN '$.ä' < 'a' THEN 1 ELSE 0 END";
        let pg = lower_pg(sql, &SqlTypeEnv::new());
        assert!(
            pg.contains("('$.ä' COLLATE \"C\")"),
            "non-json_extract '$.…' must collate: {pg}"
        );
        let sqlite = {
            let proof = proof_of(sql, &SqlTypeEnv::new());
            lower_canonical_sql_typed(DatabaseBackend::Sqlite, sql, Some(&proof)).expect("sqlite")
        };
        assert!(sqlite.contains("'$.ä'"), "{sqlite}");
        assert!(!sqlite.contains("COLLATE"), "{sqlite}");
        let env = sql_type_env_from_canonical_ddl("CREATE TABLE t (payload TEXT)");
        let extract = lower_pg("SELECT json_extract(payload, '$.ä') FROM t", &env);
        assert!(
            !extract.to_ascii_lowercase().contains("json_extract("),
            "{extract}"
        );
        assert!(extract.contains("#>>"), "{extract}");
        assert!(
            !extract.contains("('$.ä' COLLATE"),
            "json_extract path must stay a literal: {extract}"
        );
    }

    #[test]
    fn typed_lowering_fails_closed_on_malformed_sidecar() {
        let sql = "SELECT 'ä'";
        let mut proof = proof_of(sql, &SqlTypeEnv::new());
        let span = proof
            .text_collate_sites
            .first()
            .copied()
            .expect("literal collate site");
        let mid = (span.span.start + 1..span.span.end)
            .find(|&i| !sql.is_char_boundary(i))
            .expect("ä is multi-byte");
        proof.text_collate_sites[0].span.end = mid;
        let err = lower_canonical_sql_typed(DatabaseBackend::Postgres, sql, Some(&proof))
            .expect_err("mid-character TEXT span");
        assert!(
            err.to_string().contains("proof") || err.to_string().contains("UTF-8"),
            "{err}"
        );

        let add = "SELECT 1 + 2";
        let mut arith = proof_of(add, &SqlTypeEnv::new());
        arith.integer_arith_sites[0].full.end = add.len() + 8;
        let err = lower_canonical_sql_typed(DatabaseBackend::Sqlite, add, Some(&arith))
            .expect_err("out-of-range arith site");
        assert!(
            err.to_string().contains("proof") || err.to_string().contains("span"),
            "{err}"
        );
    }

    fn dense_like_json_placeholder_sql(like_count: usize) -> String {
        let mut sql = String::from(
            "SELECT json_extract(ifnull(json_object('k', body), '{}'), '$.k'), \
             json_object('k', ?) FROM t WHERE ",
        );
        for i in 0..like_count {
            if i > 0 {
                sql.push_str(" OR ");
            }
            sql.push_str("x LIKE'[%]_?*'");
        }
        sql
    }

    fn nested_helper_like_sql(like_count: usize) -> String {
        let mut sql =
            String::from("SELECT json_extract(ifnull(body, '{}'), '$.a'), 1/2 FROM t WHERE ");
        for i in 0..like_count {
            if i > 0 {
                sql.push_str(" OR ");
            }
            sql.push_str("x LIKE ?");
        }
        sql
    }

    fn insert_or_ignore_like_sql(like_count: usize) -> String {
        let mut sql = String::from("INSERT OR IGNORE INTO t SELECT 1 FROM u WHERE ");
        for i in 0..like_count {
            if i > 0 {
                sql.push_str(" OR ");
            }
            sql.push_str("x LIKE'a'");
        }
        sql
    }

    fn like_placeholder_sql(like_count: usize) -> String {
        let mut sql = String::from("SELECT json_extract(body, '$.k') FROM t WHERE ");
        for i in 0..like_count {
            if i > 0 {
                sql.push_str(" OR ");
            }
            sql.push_str("x LIKE ?");
        }
        sql
    }

    fn d1_host_accepts(sql: &str, params: &[bookclerk_plugin_abi::DbValue]) -> bool {
        let caps = bookclerk_plugin_abi::DbCapabilities::advertised_d1();
        if u32::try_from(params.len()).unwrap_or(u32::MAX) > caps.max_binds {
            return false;
        }
        if caps.admit_statement(sql, params).is_err() {
            return false;
        }
        let binds = serde_json::to_vec(params).unwrap_or_default();
        !bookclerk_plugin_abi::sql_payload_exceeds(
            sql,
            &String::from_utf8_lossy(&binds),
            caps.max_payload_bytes,
        )
    }

    fn d1_physical_after_adapter(sql: &str) -> usize {
        let trimmed = sql.trim_start();
        let capped = if trimmed.len() >= 6 && trimmed[..6].eq_ignore_ascii_case("SELECT") {
            crate::cap_query_sql(sql, bookclerk_plugin_abi::FIRST_PARTY_MAX_RESULT_ROWS)
        } else {
            sql.to_string()
        };
        lower_canonical_sql(DatabaseBackend::Sqlite, &capped).len()
    }

    #[test]
    fn like_glob_wrap_uses_abi_affixes_and_empty_trivia_overhead() {
        let pat = "a";
        let wrapped = glob_pattern_sql(pat);
        assert_eq!(
            wrapped,
            format!("{LIKE_GLOB_WRAP_PREFIX}{pat}{LIKE_GLOB_WRAP_SUFFIX}")
        );
        assert_eq!(
            wrapped.len(),
            pat.len() + bookclerk_plugin_abi::LIKE_GLOB_PATTERN_WRAP_BYTES
        );
        let empty_trivia = "SELECT 1 WHERE x LIKE'a'";
        let lowered = lower_canonical_sql(DatabaseBackend::Sqlite, empty_trivia);
        assert_eq!(
            lowered.len(),
            empty_trivia.len() + bookclerk_plugin_abi::LIKE_GLOB_REWRITE_OVERHEAD,
            "{lowered}"
        );
    }

    #[test]
    fn d1_accepted_short_likes_fit_physical_statement_limit() {
        use bookclerk_plugin_abi::{
            lowered_statement_preflight_len, lowered_statement_upper_bound_len, DbValue,
            D1_MAX_PAYLOAD_BYTES, D1_MAX_SQL_STATEMENT_BYTES,
        };

        let bind = [DbValue::Text("v".into())];
        let mut n_ok = 0usize;
        let mut n = 1usize;
        while n < 8_000 {
            let sql = dense_like_json_placeholder_sql(n);
            if d1_host_accepts(&sql, &bind) {
                let physical = d1_physical_after_adapter(&sql);
                let pre = lowered_statement_preflight_len(&sql, bind.len()).expect("preflight");
                assert!(
                    physical <= D1_MAX_SQL_STATEMENT_BYTES as usize,
                    "n={n} canonical={} physical={physical} cap={D1_MAX_PAYLOAD_BYTES}",
                    sql.len()
                );
                assert!(physical <= pre, "n={n} physical={physical} preflight={pre}");
                assert!(
                    pre <= lowered_statement_upper_bound_len(sql.len()),
                    "n={n} preflight={pre} formula={}",
                    lowered_statement_upper_bound_len(sql.len())
                );
                assert!(
                    physical <= lowered_statement_upper_bound_len(sql.len()),
                    "n={n} physical={physical} formula={}",
                    lowered_statement_upper_bound_len(sql.len())
                );
                n_ok = n;
                n += 1;
                continue;
            }
            break;
        }
        assert!(n_ok >= 1, "expected at least one accepted LIKE chain");
        let over = dense_like_json_placeholder_sql(n_ok + 1);
        assert!(
            !d1_host_accepts(&over, &bind),
            "N+1 like-count must fail host admission (payload or admit)"
        );

        let mut nested_ok = 0usize;
        let mut n = 1usize;
        while n < 8_000 {
            let sql = nested_helper_like_sql(n);
            let params: Vec<DbValue> = (0..n).map(|_| DbValue::Text("[%]_?*".into())).collect();
            if d1_host_accepts(&sql, &params) {
                let physical = d1_physical_after_adapter(&sql);
                let pre = lowered_statement_preflight_len(&sql, params.len()).expect("preflight");
                assert!(
                    physical <= D1_MAX_SQL_STATEMENT_BYTES as usize,
                    "nested n={n}"
                );
                assert!(
                    physical <= pre,
                    "nested n={n} physical={physical} pre={pre}"
                );
                nested_ok = n;
                n += 1;
                continue;
            }
            break;
        }
        assert!(nested_ok >= 1, "expected nested helper LIKE chain");
        let over_nested = nested_helper_like_sql(nested_ok + 1);
        let over_nested_params: Vec<DbValue> = (0..=nested_ok)
            .map(|_| DbValue::Text("[%]_?*".into()))
            .collect();
        assert!(
            !d1_host_accepts(&over_nested, &over_nested_params),
            "N+1 nested helper LIKE must fail host admission"
        );

        let mut insert_ok = 0usize;
        let mut n = 1usize;
        while n < 8_000 {
            let sql = insert_or_ignore_like_sql(n);
            if d1_host_accepts(&sql, &[]) {
                let physical = d1_physical_after_adapter(&sql);
                let pre = lowered_statement_preflight_len(&sql, 0).expect("preflight");
                assert!(
                    physical <= D1_MAX_SQL_STATEMENT_BYTES as usize,
                    "insert n={n}"
                );
                assert!(
                    physical <= pre,
                    "insert n={n} physical={physical} pre={pre}"
                );
                insert_ok = n;
                n += 1;
                continue;
            }
            break;
        }
        assert!(insert_ok >= 1, "expected INSERT OR IGNORE LIKE chain");
        assert!(
            !d1_host_accepts(&insert_or_ignore_like_sql(insert_ok + 1), &[]),
            "N+1 INSERT OR IGNORE LIKE must fail host admission"
        );

        let mut bind_ok = 0usize;
        let mut k = 1usize;
        while k <= bookclerk_plugin_abi::D1_MAX_BINDS as usize {
            let sql = like_placeholder_sql(k);
            let params: Vec<DbValue> = (0..k).map(|_| DbValue::Text("a".into())).collect();
            if d1_host_accepts(&sql, &params) {
                let physical = d1_physical_after_adapter(&sql);
                assert!(
                    physical <= D1_MAX_SQL_STATEMENT_BYTES as usize,
                    "placeholder n={k} physical={physical}"
                );
                bind_ok = k;
                k += 1;
                continue;
            }
            break;
        }
        assert!(bind_ok >= 1, "expected at least one accepted LIKE ? chain");
        let over_sql = like_placeholder_sql(bind_ok + 1);
        let over_params: Vec<DbValue> = (0..=bind_ok).map(|_| DbValue::Text("a".into())).collect();
        assert!(
            !d1_host_accepts(&over_sql, &over_params),
            "N+1 LIKE placeholders must fail host admission"
        );
    }

    fn dense_add_sql(adds: usize) -> String {
        let mut sql = String::from("SELECT 1");
        for _ in 0..adds {
            sql.push_str("+1");
        }
        sql
    }

    #[test]
    fn d1_overflow_n_plus_one_is_rejected_before_lowering() {
        use bookclerk_plugin_abi::{
            lowered_statement_preflight_len, lowered_statement_preflight_len_proven,
            D1_MAX_SQL_STATEMENT_BYTES,
        };
        let caps = bookclerk_plugin_abi::DbCapabilities::advertised_d1();
        let mut lo = 0usize;
        let mut hi = 1usize;
        while hi < 800 {
            let sql = dense_add_sql(hi);
            if d1_host_accepts(&sql, &[]) {
                lo = hi;
                hi = (hi.saturating_mul(2)).min(800);
            } else {
                break;
            }
        }
        while lo + 1 < hi {
            let mid = lo + (hi - lo) / 2;
            if d1_host_accepts(&dense_add_sql(mid), &[]) {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        let n_ok = lo;
        assert!(n_ok >= 1, "expected an admitted add chain");
        let sql = dense_add_sql(n_ok);
        let proof = proof_of(&sql, &SqlTypeEnv::new());
        let pre = lowered_statement_preflight_len_proven(&sql, 0, Some(&proof)).expect("preflight");
        let lowered =
            lower_canonical_sql_typed(DatabaseBackend::Sqlite, &sql, Some(&proof)).expect("lower");
        let physical =
            crate::cap_query_sql(&lowered, bookclerk_plugin_abi::FIRST_PARTY_MAX_RESULT_ROWS).len();
        assert!(physical <= pre, "physical={physical} pre={pre}");
        assert!(
            physical <= D1_MAX_SQL_STATEMENT_BYTES as usize,
            "physical={physical}"
        );
        let over = dense_add_sql(n_ok + 1);
        assert!(
            !d1_host_accepts(&over, &[]),
            "N+1 add chain must fail portable admission"
        );
        let mechanical = lowered_statement_preflight_len(&over, 0).expect("mechanical");
        assert!(
            mechanical <= D1_MAX_SQL_STATEMENT_BYTES as usize,
            "N+1 still fits mechanical-only ({mechanical})"
        );
        let proof = proof_of(&over, &SqlTypeEnv::new());
        let pre = lowered_statement_preflight_len_proven(&over, 0, Some(&proof)).expect("pre N+1");
        assert!(
            pre > D1_MAX_SQL_STATEMENT_BYTES as usize || caps.admit_statement(&over, &[]).is_err(),
            "N+1 proven preflight {pre} must miss the physical ceiling"
        );
    }
}

#[cfg(test)]
mod proptests {
    use super::*;
    use bookclerk_plugin_abi::SqlTypeEnv;
    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig {
            cases: 64,
            ..ProptestConfig::default()
        })]

        #[test]
        fn lower_never_panics_on_arbitrary_text(s in r"[\x01-\x7f]{0,180}") {
            let _ = lower_canonical_sql(DatabaseBackend::Sqlite, &s);
            let _ = lower_canonical_sql(DatabaseBackend::Postgres, &s);
        }

        #[test]
        fn string_and_comment_semicolons_are_copied_verbatim(inner in r"[^\x00'*/\n]{0,24}") {
            let sql = format!(
                "SELECT '{inner};lit' AS x, /* {inner};block */ 1 -- {inner};line\nFROM t"
            );
            let sqlite = lower_canonical_sql(DatabaseBackend::Sqlite, &sql);
            let postgres = lower_canonical_sql(DatabaseBackend::Postgres, &sql);
            prop_assert!(sqlite.contains(&format!("'{inner};lit'")), "{sqlite}");
            prop_assert!(postgres.contains(&format!("'{inner};lit'")), "{postgres}");
            prop_assert!(sqlite.contains(&format!("/* {inner};block */")), "{sqlite}");
            prop_assert!(postgres.contains(&format!("/* {inner};block */")), "{postgres}");
            prop_assert!(sqlite.contains(&format!("-- {inner};line")), "{sqlite}");
            prop_assert!(postgres.contains(&format!("-- {inner};line")), "{postgres}");
        }

        #[test]
        fn proof_hash_mismatch_fails_closed(extra in r"[a-z]{1,8}") {
            let sql = "SELECT 'ok'";
            let proof = {
                use bookclerk_plugin_abi::{
                    typecheck_execute_request_proofs, DbPlanStatementKind, DbResultSelection,
                    ExecuteRequest, TypedDbStatement,
                };
                let req = ExecuteRequest {
                    operation_id: "p".into(),
                    request_hash: String::new(),
                    statements: vec![TypedDbStatement {
                        sql: sql.into(),
                        parameters: vec![],
                        kind: DbPlanStatementKind::Select,
                        max_rows: 0,
                        result_selection: DbResultSelection::Rows,
                    }],
                    deadline_unix_ms: 0,
                };
                typecheck_execute_request_proofs(&req, &SqlTypeEnv::new())
                    .expect("typecheck")
                    .into_iter()
                    .next()
                    .expect("proof")
            };
            let mutated = format!("{sql} /* {extra} */");
            let err = lower_canonical_sql_typed(
                DatabaseBackend::Sqlite,
                &mutated,
                Some(&proof),
            )
            .expect_err("hash mismatch");
            prop_assert!(
                err.to_string().contains("proof") || err.to_string().contains("bound"),
                "{err}"
            );
        }
    }
}
