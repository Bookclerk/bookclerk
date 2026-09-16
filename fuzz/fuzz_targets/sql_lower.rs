//! Coverage-guided physical lowering. Quoted strings/comments must be copied
//! verbatim except documented LIKE→GLOB / helper rewrites.

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
    assert_quoted_semicolons_preserved(sql, &sqlite);
    assert_quoted_semicolons_preserved(sql, &postgres);
    if !sql.to_ascii_uppercase().contains("LIKE") {
        for lit in quoted_strings(sql) {
            assert!(
                sqlite.contains(&lit) && postgres.contains(&lit),
                "lowerer rewrote string {lit:?}"
            );
        }
    }
    let _ = bookclerk_db_exec::lower_canonical_sql_typed(DatabaseBackend::Sqlite, sql, None);
    let _ = bookclerk_db_exec::lower_canonical_sql_typed(DatabaseBackend::Postgres, sql, None);
});

fn quoted_strings(sql: &str) -> Vec<String> {
    let bytes = sql.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\'' {
            let start = i;
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\'' {
                    if bytes.get(i + 1) == Some(&b'\'') {
                        i += 2;
                        continue;
                    }
                    i += 1;
                    out.push(sql[start..i].to_string());
                    break;
                }
                i += 1;
            }
            continue;
        }
        i += 1;
    }
    out
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
