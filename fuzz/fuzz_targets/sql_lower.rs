//! Coverage-guided physical lowering. Quoted strings/comments must be copied
//! verbatim except documented LIKE→GLOB / helper rewrites (Postgres
//! `json_extract` paths and `json(CASE …)` booleans).

#![no_main]

use libfuzzer_sys::fuzz_target;
use sea_orm::DatabaseBackend;

fuzz_target!(|data: &[u8]| {
    if data.len() > 2048 {
        return;
    }
    let Ok(sql) = std::str::from_utf8(data) else {
        return;
    };
    let sqlite = bookclerk_db_exec::lower_canonical_sql(DatabaseBackend::Sqlite, sql);
    let postgres = bookclerk_db_exec::lower_canonical_sql(DatabaseBackend::Postgres, sql);
    let upper = sql.to_ascii_uppercase();
    // Postgres helper rewrites consume string literals by design: `json_extract`
    // paths ('$.a' becomes a `#>>` path array) and the `json(CASE … 'true' …)`
    // boolean helpers. Those inputs check strings on SQLite only.
    let postgres_rewrites_literals = upper.contains("JSON_EXTRACT") || upper.contains("JSON(");
    assert_quoted_semicolons_preserved(sql, &sqlite);
    if !postgres_rewrites_literals {
        assert_quoted_semicolons_preserved(sql, &postgres);
    }
    if !upper.contains("LIKE") {
        for lit in quoted_strings(sql) {
            assert!(
                sqlite.contains(&lit) && (postgres_rewrites_literals || postgres.contains(&lit)),
                "lowerer rewrote string {lit:?}"
            );
        }
    }
    let _ = bookclerk_db_exec::lower_canonical_sql_typed(DatabaseBackend::Sqlite, sql, None);
    let _ = bookclerk_db_exec::lower_canonical_sql_typed(DatabaseBackend::Postgres, sql, None);
});

/// Returns every single-quoted literal in `sql`, skipping `"` identifiers,
/// `$tag$` dollar-quotes and `--` / `/* */` comments so a quote inside them is
/// not read as a literal.
fn quoted_strings(sql: &str) -> Vec<String> {
    let bytes = sql.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            quote @ (b'\'' | b'"') => {
                let start = i;
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == quote {
                        if bytes.get(i + 1) == Some(&quote) {
                            i += 2;
                            continue;
                        }
                        i += 1;
                        if quote == b'\'' {
                            out.push(sql[start..i].to_string());
                        }
                        break;
                    }
                    i += 1;
                }
            }
            b'$' => i += dollar_quote_len(&sql[i..]).unwrap_or(1),
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i < bytes.len() && !(bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/')) {
                    i += 1;
                }
                i += 2;
            }
            _ => i += 1,
        }
    }
    out
}

/// Length of a `$tag$…$tag$` block at `s` (which starts with `$`), if closed.
fn dollar_quote_len(s: &str) -> Option<usize> {
    let rest = s.get(1..)?;
    let tag_end = rest.find('$').filter(|&end| {
        rest[..end]
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
    })?;
    let tag = &s[..tag_end + 2];
    let close = s[tag.len()..].find(tag)?;
    Some(tag.len() + close + tag.len())
}

fn assert_quoted_semicolons_preserved(src: &str, lowered: &str) {
    for lit in quoted_strings(src) {
        if lit.contains(';') {
            assert!(
                lowered.contains(&lit),
                "semicolon in string was rewritten: {lit} / {lowered}"
            );
        }
    }
}
