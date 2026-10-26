//! Replay checked-in cargo-fuzz corpora in PR CI (no libFuzzer required).

use sea_orm::DatabaseBackend;

#[test]
fn fuzz_corpus_sql_lower_does_not_panic() {
    let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fuzz/corpus/sql_lower")
        .canonicalize()
        .expect("canonicalize fuzz corpus");
    for entry in std::fs::read_dir(&dir).expect("fuzz corpus") {
        let entry = entry.expect("entry");
        // Inline canonicalize → starts_with at each sink (CodeQL two-state barrier).
        let Ok(path) = entry.path().canonicalize() else {
            continue;
        };
        if !path.starts_with(&dir) {
            continue;
        }
        if !path.is_file() {
            continue;
        }
        let sql = std::fs::read_to_string(&path).expect("read");
        let sqlite = bookclerk_db_exec::lower_canonical_sql(DatabaseBackend::Sqlite, &sql);
        let postgres = bookclerk_db_exec::lower_canonical_sql(DatabaseBackend::Postgres, &sql);
        if sql.contains(';') && sql.contains('\'') {
            assert!(
                sqlite.contains(';') && postgres.contains(';'),
                "{} lost semicolon: {sqlite} / {postgres}",
                path.display()
            );
        }
        let upper = sql.to_ascii_uppercase();
        if !upper.contains("LIKE") && !upper.contains("JSON_EXTRACT") && !upper.contains("JSON(") {
            for lit in quoted_strings(&sql) {
                assert!(
                    sqlite.contains(&lit) && postgres.contains(&lit),
                    "{} rewrote string {lit:?}: {postgres:?}",
                    path.display()
                );
            }
        }
        bookclerk_db_exec::lower_canonical_sql_typed(DatabaseBackend::Sqlite, &sql, None).ok();
        bookclerk_db_exec::lower_canonical_sql_typed(DatabaseBackend::Postgres, &sql, None).ok();
    }
}

/// Returns every single-quoted literal in `sql`, the same oracle the
/// `sql_lower` fuzz target uses.
///
/// Double-quoted identifiers, `$tag$` dollar quotes and `--` / `/* */`
/// comments are skipped, so a quote inside them is not read as a literal. The
/// lowerer may rewrite literals only under `LIKE` and the Postgres JSON
/// helpers; everywhere else each literal must survive byte for byte.
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
