//! Host semantic desugars for canonical Bookclerk SQL.
#![allow(clippy::missing_docs_in_private_items)]
//!
//! These rewrites are **backend-independent** and define Bookclerk behavior:
//! unspecified `ORDER BY` matches SQLite NULL ordering, and `/` `%` by zero
//! yield NULL via portable `NULLIF`. Adapters must not repeat them (doing so
//! would double-wrap `NULLIF` and shift proof spans).

use crate::ExecuteRequest;

/// Applies host semantic desugars to canonical SQL.
///
/// Explicit `NULLS FIRST`/`LAST` and a divisor that is structurally
/// `NULLIF(<expression>, 0)` are left unchanged. A `NULLIF` call whose second
/// argument is not the integer `0` (for example `NULLIF(0, 1)`) still gains
/// an outer zero guard. Callers must run this **once** before proof
/// generation; adapters must not repeat it.
#[must_use]
pub fn desugar_canonical_sql(sql: &str) -> String {
    let sql = rewrite_div_mod_null_on_zero(sql);
    rewrite_order_by_nulls(&sql)
}

/// Desugars every statement in `req` in place.
pub fn desugar_execute_request(req: &mut ExecuteRequest) {
    for stmt in &mut req.statements {
        stmt.sql = desugar_canonical_sql(&stmt.sql);
    }
}

/// Portable `/` and `%` by zero: wrap the divisor as `NULLIF(x, 0)`.
fn rewrite_div_mod_null_on_zero(sql: &str) -> String {
    let mut i = 0;
    let mut out = String::with_capacity(sql.len() + 16);
    while i < sql.len() {
        if let Some(len) = literal_or_comment_len(&sql[i..]) {
            out.push_str(&sql[i..i + len]);
            i += len;
            continue;
        }
        let ch = sql[i..].chars().next().unwrap_or('\0');
        if ch == '/' || ch == '%' {
            out.push(ch);
            i += ch.len_utf8();
            let j = skip_trivia_idx(sql, i);
            out.push_str(&sql[i..j]);
            if let Some((atom_end, atom)) = take_div_operand(sql, j) {
                if operand_already_nullif(atom) {
                    out.push_str(atom);
                } else {
                    out.push_str("NULLIF(");
                    out.push_str(atom);
                    out.push_str(", 0)");
                }
                i = atom_end;
                continue;
            }
            // Trivia after `/` or `%` was already copied. Advance past it so an
            // unparsable operand cannot duplicate whitespace or comments.
            i = j;
            continue;
        }
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// True when `atom` is structurally `NULLIF(<expression>, 0)`.
///
/// Any other `NULLIF` call (including `NULLIF(0, 1)`, which evaluates to `0`)
/// still needs an outer zero guard so `/` and `%` stay portable.
fn operand_already_nullif(atom: &str) -> bool {
    let s = unwrap_outer_parens(atom);
    let i = skip_trivia_idx(s, 0);
    if !ident_eq_ci(s, i, "NULLIF") {
        return false;
    }
    let after_name = skip_trivia_idx(s, i + "NULLIF".len());
    if s.as_bytes().get(after_name) != Some(&b'(') {
        return false;
    }
    let close = skip_balanced(s, after_name);
    if close < after_name.saturating_add(2) {
        return false;
    }
    if skip_trivia_idx(s, close) != s.len() {
        return false;
    }
    let inside = &s[after_name + 1..close - 1];
    let Some(args) = split_top_level_args(inside) else {
        return false;
    };
    args.len() == 2 && is_integer_zero(args[1])
}

/// Strips matching outer parentheses and surrounding whitespace.
fn unwrap_outer_parens(atom: &str) -> &str {
    let mut cur = atom;
    loop {
        let start = skip_trivia_idx(cur, 0);
        let mut end = cur.len();
        while end > start {
            let Some(ch) = cur[..end].chars().next_back() else {
                break;
            };
            if ch.is_whitespace() {
                end -= ch.len_utf8();
            } else {
                break;
            }
        }
        if start >= end {
            return "";
        }
        let span = &cur[start..end];
        if span.as_bytes().first() != Some(&b'(') || span.as_bytes().last() != Some(&b')') {
            return span;
        }
        let close = skip_balanced(span, 0);
        if close != span.len() {
            return span;
        }
        cur = &span[1..span.len() - 1];
    }
}

/// Top-level comma-separated arguments inside a call (literals/comments skipped).
fn split_top_level_args(inside: &str) -> Option<Vec<&str>> {
    let mut args = Vec::new();
    let mut start = 0usize;
    let mut depth = 0i32;
    let mut i = 0usize;
    while i < inside.len() {
        if let Some(len) = literal_or_comment_len(&inside[i..]) {
            i += len;
            continue;
        }
        let ch = inside[i..].chars().next()?;
        match ch {
            '(' => depth += 1,
            ')' if depth == 0 => return None,
            ')' => depth -= 1,
            ',' if depth == 0 => {
                args.push(&inside[start..i]);
                start = i + 1;
            }
            _ => {}
        }
        i += ch.len_utf8();
    }
    if depth != 0 {
        return None;
    }
    args.push(&inside[start..]);
    Some(args)
}

/// True when `arg` is the integer token `0` (optional parens and trivia).
fn is_integer_zero(arg: &str) -> bool {
    let s = unwrap_outer_parens(arg);
    let i = skip_trivia_idx(s, 0);
    if s.as_bytes().get(i) != Some(&b'0') {
        return false;
    }
    let after = i + 1;
    if let Some(&b) = s.as_bytes().get(after) {
        if b.is_ascii_digit() || b == b'.' || b == b'e' || b == b'E' || b == b'x' || b == b'X' {
            return false;
        }
    }
    skip_trivia_idx(s, after) == s.len()
}

/// Operand of `/` or `%`: admitted primary (paren, ident, call, CAST, unary, number, bind).
fn take_div_operand(sql: &str, start: usize) -> Option<(usize, &str)> {
    let s = skip_trivia_idx(sql, start);
    let end = take_div_primary(sql, s)?;
    if end > s {
        Some((end, &sql[s..end]))
    } else {
        None
    }
}

fn take_div_primary(sql: &str, start: usize) -> Option<usize> {
    let s = skip_trivia_idx(sql, start);
    let bytes = sql.as_bytes();
    if bytes.get(s) == Some(&b'(') {
        return Some(skip_balanced(sql, s));
    }
    if bytes.get(s) == Some(&b'?') {
        return Some(s + 1);
    }
    if bytes.get(s) == Some(&b'+') || bytes.get(s) == Some(&b'-') {
        return take_div_primary(sql, s + 1);
    }
    if let Some((_, end)) = ident_span_at(sql, s) {
        let mut j = end;
        loop {
            let k = skip_trivia_idx(sql, j);
            if bytes.get(k) == Some(&b'.') {
                let k2 = skip_trivia_idx(sql, k + 1);
                if let Some((_, e2)) = ident_span_at(sql, k2) {
                    j = e2;
                    continue;
                }
            }
            if bytes.get(k) == Some(&b'(') {
                return Some(skip_balanced(sql, k));
            }
            return Some(j);
        }
    }
    let mut i = s;
    if i < bytes.len() && bytes[i].is_ascii_digit() {
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if bytes.get(i) == Some(&b'.') {
            i += 1;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
        }
        return Some(i);
    }
    None
}

/// Appends SQLite-equivalent NULL ordering (`ASC NULLS FIRST`, `DESC NULLS LAST`).
fn rewrite_order_by_nulls(sql: &str) -> String {
    let mut i = 0;
    let mut out = String::with_capacity(sql.len() + 32);
    while i < sql.len() {
        if let Some(len) = literal_or_comment_len(&sql[i..]) {
            out.push_str(&sql[i..i + len]);
            i += len;
            continue;
        }
        if ident_eq_ci(sql, i, "ORDER") {
            let after_order = skip_trivia_idx(sql, i + "ORDER".len());
            if ident_eq_ci(sql, after_order, "BY") {
                let by_end = after_order + "BY".len();
                out.push_str(&sql[i..by_end]);
                i = rewrite_order_by_items(sql, by_end, &mut out);
                continue;
            }
        }
        let ch = sql[i..].chars().next().unwrap_or('\0');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Copies one `ORDER BY` item list, inserting NULLS FIRST/LAST when omitted.
fn rewrite_order_by_items(sql: &str, mut i: usize, out: &mut String) -> usize {
    loop {
        let start = i;
        i = skip_trivia_idx(sql, i);
        out.push_str(&sql[start..i]);
        if i >= sql.len() {
            return i;
        }
        let expr_end = skip_order_by_expr(sql, i);
        out.push_str(&sql[i..expr_end]);
        i = skip_trivia_idx(sql, expr_end);
        let mut desc = false;
        if ident_eq_ci(sql, i, "ASC") {
            out.push(' ');
            out.push_str(&sql[i..i + "ASC".len()]);
            i = skip_trivia_idx(sql, i + "ASC".len());
        } else if ident_eq_ci(sql, i, "DESC") {
            out.push(' ');
            out.push_str(&sql[i..i + "DESC".len()]);
            i = skip_trivia_idx(sql, i + "DESC".len());
            desc = true;
        }
        if ident_eq_ci(sql, i, "NULLS") {
            let after_nulls = skip_trivia_idx(sql, i + "NULLS".len());
            if ident_eq_ci(sql, after_nulls, "FIRST") || ident_eq_ci(sql, after_nulls, "LAST") {
                let kw_len = if ident_eq_ci(sql, after_nulls, "FIRST") {
                    "FIRST".len()
                } else {
                    "LAST".len()
                };
                out.push(' ');
                out.push_str(&sql[i..after_nulls + kw_len]);
                i = skip_trivia_idx(sql, after_nulls + kw_len);
            }
        } else if desc {
            out.push_str(" NULLS LAST");
        } else {
            out.push_str(" NULLS FIRST");
        }
        if let Some(next) = sql[i..].chars().next() {
            if !next.is_whitespace() && next != ',' && next != ';' && next != ')' {
                out.push(' ');
            }
        }
        if sql.as_bytes().get(i) == Some(&b',') {
            out.push(',');
            i += 1;
            continue;
        }
        return i;
    }
}

/// Byte offset after one `ORDER BY` expression (balanced parens, literals skipped).
fn skip_order_by_expr(sql: &str, mut i: usize) -> usize {
    let mut depth = 0i32;
    while i < sql.len() {
        if let Some(len) = literal_or_comment_len(&sql[i..]) {
            i += len;
            continue;
        }
        let ch = sql[i..].chars().next().unwrap_or('\0');
        if ch == '(' {
            depth += 1;
            i += 1;
            continue;
        }
        if ch == ')' {
            if depth == 0 {
                return i;
            }
            depth -= 1;
            i += 1;
            continue;
        }
        if depth == 0 {
            if ch == ',' || ch == ';' {
                return i;
            }
            if ch.is_whitespace() {
                let next = skip_trivia_idx(sql, i);
                if order_by_item_terminator(sql, next) {
                    return i;
                }
            }
            if order_by_item_terminator(sql, i) {
                return i;
            }
        }
        i += ch.len_utf8();
    }
    i
}

/// True when `ORDER BY` item parsing should stop (`ASC`/`LIMIT`/…).
fn order_by_item_terminator(sql: &str, i: usize) -> bool {
    ident_eq_ci(sql, i, "ASC")
        || ident_eq_ci(sql, i, "DESC")
        || ident_eq_ci(sql, i, "NULLS")
        || ident_eq_ci(sql, i, "LIMIT")
        || ident_eq_ci(sql, i, "OFFSET")
        || ident_eq_ci(sql, i, "RETURNING")
        || ident_eq_ci(sql, i, "UNION")
        || ident_eq_ci(sql, i, "EXCEPT")
        || ident_eq_ci(sql, i, "INTERSECT")
        || ident_eq_ci(sql, i, "FETCH")
        || ident_eq_ci(sql, i, "FOR")
        || ident_eq_ci(sql, i, "WINDOW")
}

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

fn word_boundary(sql: &str, idx: Option<usize>) -> bool {
    match idx.and_then(|i| sql.as_bytes().get(i)) {
        Some(b) => !(b.is_ascii_alphanumeric() || *b == b'_'),
        None => true,
    }
}

fn ident_eq_ci(sql: &str, i: usize, word: &str) -> bool {
    let Some(end) = i.checked_add(word.len()).filter(|e| *e <= sql.len()) else {
        return false;
    };
    sql.get(i..end)
        .is_some_and(|s| s.eq_ignore_ascii_case(word))
        && word_boundary(sql, i.checked_sub(1))
        && word_boundary(sql, Some(end))
}

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

fn literal_or_comment_len(s: &str) -> Option<usize> {
    comment_len(s).or_else(|| quoted_len(s))
}

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
#[allow(clippy::missing_panics_doc)]
mod tests {
    use super::*;

    #[test]
    fn unspecified_order_by_gets_sqlite_nulls() {
        assert_eq!(
            desugar_canonical_sql("SELECT a FROM t ORDER BY a"),
            "SELECT a FROM t ORDER BY a NULLS FIRST"
        );
        assert_eq!(
            desugar_canonical_sql("SELECT a FROM t ORDER BY a DESC"),
            "SELECT a FROM t ORDER BY a DESC NULLS LAST"
        );
        assert_eq!(
            desugar_canonical_sql("SELECT a FROM t ORDER BY a, b DESC"),
            "SELECT a FROM t ORDER BY a NULLS FIRST, b DESC NULLS LAST"
        );
        assert_eq!(
            desugar_canonical_sql("SELECT a FROM t ORDER BY a LIMIT 1"),
            "SELECT a FROM t ORDER BY a NULLS FIRST LIMIT 1"
        );
    }

    #[test]
    fn explicit_nulls_are_idempotent() {
        let sql = "SELECT a FROM t ORDER BY a ASC NULLS LAST";
        assert_eq!(desugar_canonical_sql(sql), sql);
        assert_eq!(desugar_canonical_sql(&desugar_canonical_sql(sql)), sql);
    }

    #[test]
    fn div_mod_wrap_divisor_in_nullif() {
        let sql = desugar_canonical_sql("SELECT 1 / 0, 4 % 0, a / b");
        assert!(sql.contains("NULLIF(0, 0)"), "{sql}");
        assert!(sql.contains("NULLIF(b, 0)"), "{sql}");
        assert_eq!(
            desugar_canonical_sql(&sql),
            sql,
            "NULLIF wrap is idempotent"
        );
    }

    #[test]
    fn div_operand_covers_call_qualified_unary_and_cast() {
        let sql = desugar_canonical_sql(
            "SELECT 10 / abs(n), 10 / t.n, 10 / -n, 10 / CAST(n AS INTEGER), 10 / (n + 1)",
        );
        assert!(sql.contains("NULLIF(abs(n), 0)"), "{sql}");
        assert!(sql.contains("NULLIF(t.n, 0)"), "{sql}");
        assert!(sql.contains("NULLIF(-n, 0)"), "{sql}");
        assert!(sql.contains("NULLIF(CAST(n AS INTEGER), 0)"), "{sql}");
        assert!(sql.contains("NULLIF((n + 1), 0)"), "{sql}");
    }

    #[test]
    fn literals_are_not_rewritten() {
        let sql = "SELECT 'a / b ORDER BY x' FROM t";
        assert_eq!(desugar_canonical_sql(sql), sql);
    }

    #[test]
    fn unparsable_div_operand_does_not_duplicate_trivia() {
        let sql = "SELECT 1 /  /*c*/ * FROM t";
        let out = desugar_canonical_sql(sql);
        assert_eq!(out, sql, "trivia after `/` must be copied once: {out}");
        assert_eq!(out.matches("/*c*/").count(), 1, "{out}");
        let spaces = "SELECT 1 /   * FROM t";
        assert_eq!(desugar_canonical_sql(spaces), spaces);
    }

    #[test]
    fn nullif_zero_guard_is_structural_not_name_only() {
        let already = desugar_canonical_sql("SELECT 1 / NULLIF(x, 0)");
        assert_eq!(already, "SELECT 1 / NULLIF(x, 0)");
        assert_eq!(desugar_canonical_sql(&already), already);

        let div = desugar_canonical_sql("SELECT 1 / NULLIF(0, 1)");
        assert_eq!(div, "SELECT 1 / NULLIF(NULLIF(0, 1), 0)");
        assert_eq!(desugar_canonical_sql(&div), div, "must stay idempotent");

        let modulo = desugar_canonical_sql("SELECT 1 % NULLIF(0, 1)");
        assert_eq!(modulo, "SELECT 1 % NULLIF(NULLIF(0, 1), 0)");
        assert_eq!(desugar_canonical_sql(&modulo), modulo);

        let pair = desugar_canonical_sql("SELECT 1 / NULLIF(0, 1), 1 % NULLIF(0, 1)");
        assert_eq!(
            pair,
            "SELECT 1 / NULLIF(NULLIF(0, 1), 0), 1 % NULLIF(NULLIF(0, 1), 0)"
        );
        assert_eq!(desugar_canonical_sql(&pair), pair);

        let folded = desugar_canonical_sql("SELECT 1 / nullif(0, 1)");
        assert_eq!(folded, "SELECT 1 / NULLIF(nullif(0, 1), 0)");
        assert_eq!(desugar_canonical_sql(&folded), folded);

        let spaced = desugar_canonical_sql("SELECT 1 /  NULLIF ( 0 , 1 )");
        assert!(spaced.contains("NULLIF(NULLIF ( 0 , 1 ), 0)"), "{spaced}");
        assert_eq!(desugar_canonical_sql(&spaced), spaced);

        let comments = desugar_canonical_sql("SELECT 1 / NULLIF(/*z*/ 0, /*w*/ 1)");
        assert_eq!(comments, "SELECT 1 / NULLIF(NULLIF(/*z*/ 0, /*w*/ 1), 0)");
        assert_eq!(desugar_canonical_sql(&comments), comments);

        let trivia = desugar_canonical_sql("SELECT 1 / /*c*/ NULLIF(0, 1)");
        assert_eq!(trivia, "SELECT 1 / /*c*/ NULLIF(NULLIF(0, 1), 0)");
        assert_eq!(desugar_canonical_sql(&trivia), trivia);

        let parens = desugar_canonical_sql("SELECT 1 / (NULLIF(0, 1))");
        assert_eq!(parens, "SELECT 1 / NULLIF((NULLIF(0, 1)), 0)");
        assert_eq!(desugar_canonical_sql(&parens), parens);

        let already_parens = desugar_canonical_sql("SELECT 1 / (NULLIF(x, 0))");
        assert_eq!(already_parens, "SELECT 1 / (NULLIF(x, 0))");
        assert_eq!(desugar_canonical_sql(&already_parens), already_parens);

        let already_inner_zero = desugar_canonical_sql("SELECT 1 / NULLIF(0, 0)");
        assert_eq!(already_inner_zero, "SELECT 1 / NULLIF(0, 0)");

        let nested = desugar_canonical_sql("SELECT 1 / NULLIF(NULLIF(0, 1), 0)");
        assert_eq!(nested, "SELECT 1 / NULLIF(NULLIF(0, 1), 0)");
    }
}
