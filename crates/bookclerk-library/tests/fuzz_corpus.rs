//! Replays the checked-in `guest_sql_authz` fuzz corpus in PR CI (no libFuzzer
//! required) with the fuzz target's oracle: every statement the guest SQL
//! authorizer admits is compiled under SQLite's authorizer callback and must
//! touch only what its policy grants (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The oracle lives in `fuzz/oracles/guest_sql_authz.rs` and is included here
//! with `#[path]`, so this test and the fuzz target cannot drift apart. It
//! lives in this crate because the host entry point
//! (`sql_plan::authorize_guest_typed_request`) and the SQLite oracle
//! dependency are both here.
// Test code: a failed assertion or a missing corpus file is the test failing,
// not a documented panic path.
#![allow(clippy::missing_panics_doc)]

use std::path::{Path, PathBuf};

#[path = "../../../fuzz/oracles/guest_sql_authz.rs"]
#[allow(dead_code)]
mod oracle;

use oracle::{Access, Variant};

/// Regular files directly under `fuzz/corpus/<target>/`, canonicalized and
/// confined to that directory.
// Test helper: a missing corpus is a failed test, not a handled error.
#[allow(clippy::expect_used)]
fn corpus_files(target: &str) -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fuzz/corpus")
        .join(target)
        .canonicalize()
        .expect("canonicalize fuzz corpus");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("fuzz corpus")
        .map(|entry| entry.expect("entry").path())
        // Inline canonicalize → starts_with at each sink (CodeQL two-state barrier).
        .filter_map(|path| path.canonicalize().ok())
        .filter(|path| path.starts_with(&dir) && path.is_file())
        .collect();
    files.sort();
    assert!(!files.is_empty(), "empty corpus for {target}");
    files
}

/// Seeds whose file name says the host must refuse them; every other seed
/// must be admitted, so a regression in either direction fails the replay.
fn must_refuse(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            name.starts_with("regression_")
                || name.contains("denied")
                || name.contains("refused")
                || name.contains("hidden")
                || name.contains("reserved")
                || name.contains("_star")
        })
}

/// The host's own verdict on a seed, for failure messages.
fn host_verdict(data: &[u8]) -> String {
    let Some((variant, sql, text_binds)) = oracle::decode(data) else {
        return "seed is empty, too long or not UTF-8".to_string();
    };
    match oracle::host_admits(variant, sql, text_binds) {
        Ok(()) => format!("{variant:?} admitted it"),
        Err(err) => format!("{variant:?} refused it: {err}"),
    }
}

#[test]
fn fuzz_corpus_guest_sql_authz_stays_inside_policy() {
    let mut admitted = 0;
    for path in corpus_files("guest_sql_authz") {
        let data = std::fs::read(&path).expect("read");
        let was_admitted = oracle::check(&data);
        if was_admitted {
            admitted += 1;
        }
        assert_eq!(
            was_admitted,
            !must_refuse(&path),
            "seed {} must be {}; host said: {}",
            path.display(),
            if must_refuse(&path) {
                "refused"
            } else {
                "admitted"
            },
            host_verdict(&data)
        );
    }
    assert!(admitted > 0, "no guest_sql_authz seed was admitted");
}

#[test]
fn sqlite_oracle_sees_through_subqueries_and_ctes() {
    let accesses = oracle::sqlite_accesses(
        "WITH recent AS (SELECT id FROM books) SELECT title FROM books WHERE id IN \
         (SELECT id FROM recent) AND id IN (SELECT id FROM host_secrets)",
    );
    assert!(accesses.contains(&Access::Table {
        table: "host_secrets".into(),
        column: Some("id".into()),
    }));
    assert!(accesses.contains(&Access::Table {
        table: "books".into(),
        column: Some("title".into()),
    }));
    assert!(
        oracle::violation(Variant::Lexical, &accesses).is_some(),
        "a read of host_secrets must be a violation under the lexical policy"
    );
}

#[test]
fn every_variant_admits_a_plain_granted_select() {
    for variant in Variant::ALL {
        assert!(
            oracle::host_admits(variant, "SELECT id, title FROM books WHERE id = ?", false).is_ok(),
            "{variant:?}"
        );
        let accesses = oracle::sqlite_accesses("SELECT id, title FROM books WHERE id = ?");
        assert_eq!(oracle::violation(variant, &accesses), None, "{variant:?}");
    }
}
