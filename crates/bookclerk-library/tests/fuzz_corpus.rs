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
fn unknown_tables_are_ungranted_not_aliases() {
    // A table SQLite cannot resolve is never a CTE alias, so an admitted
    // statement over it is a violation for every non-binding variant; a
    // binding variant owns tables the oracle schema does not list and is held
    // only to the reserved-name rule.
    let accesses = oracle::sqlite_accesses("SELECT * FROM arbitrary_host_table");
    assert!(accesses.contains(&Access::MissingTable("arbitrary_host_table".into())));
    for variant in [Variant::Lexical, Variant::ColumnRestricted, Variant::Typed] {
        assert!(
            oracle::violation(variant, &accesses).is_some(),
            "{variant:?}"
        );
    }
    // A name that only qualifies a column or `*` was never a table reference:
    // SQLite reports the same "no such table", but nothing is read.
    let dangling = oracle::sqlite_accesses("SELECT x.*, title FROM books");
    assert!(
        !dangling
            .iter()
            .any(|a| matches!(a, Access::MissingTable(_))),
        "{dangling:?}"
    );
    let commented = oracle::sqlite_accesses("SELECT x.*, title FROM books -- x* 'x'");
    assert!(
        !commented
            .iter()
            .any(|a| matches!(a, Access::MissingTable(_))),
        "{commented:?}"
    );
    let mixed = oracle::sqlite_accesses("SELECT x.* FROM x");
    assert!(
        mixed.contains(&Access::MissingTable("x".into())),
        "{mixed:?}"
    );
    let upper = oracle::sqlite_accesses("SELECT B2.*, title FROM books");
    assert!(
        !upper.iter().any(|a| matches!(a, Access::MissingTable(_))),
        "{upper:?}"
    );
    let aliased = oracle::sqlite_accesses("SELECT b.*, 1 AS b -- b*");
    assert!(
        !aliased.iter().any(|a| matches!(a, Access::MissingTable(_))),
        "{aliased:?}"
    );
    // The probe's scratch table is rolled back, so the next statement over
    // the same name still finds it missing.
    let again = oracle::sqlite_accesses("SELECT * FROM x");
    assert!(
        again.contains(&Access::MissingTable("x".into())),
        "{again:?}"
    );
    for variant in [Variant::BindingOwned, Variant::BindingMigration] {
        assert_eq!(oracle::violation(variant, &accesses), None, "{variant:?}");
        assert!(
            oracle::violation(variant, &[Access::MissingTable("bookclerk_private".into())])
                .is_some()
        );
    }
    // A binding is its own database: a host-library name is not reserved
    // there, exactly as the host rule has it (DDL is still a migration-only
    // action).
    let drop = oracle::sqlite_accesses("DROP TABLE IF EXISTS encrypted_secrets");
    assert_eq!(oracle::violation(Variant::BindingMigration, &drop), None);
    assert!(oracle::violation(Variant::BindingOwned, &drop).is_some());
    // SQLite serves DDL from its own catalog, which the authorizer reports as
    // sqlite_master reads; a statement that names the catalog is a violation
    // under every variant.
    assert!(drop
        .iter()
        .any(|a| matches!(a, Access::Table { table, .. } if table == "sqlite_master")));
    let catalog = oracle::sqlite_accesses("SELECT name FROM sqlite_master");
    for variant in Variant::ALL {
        assert!(
            oracle::violation(variant, &catalog).is_some(),
            "{variant:?}"
        );
    }
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
        // The typed variants lower with the proof, so overflow rewrite sites
        // reach SQLite; the lexical ones lower mechanically.
        // Trailing whitespace: the adapter lowers the trimmed statement.
        let arith = "SELECT id + 1, price * 2 FROM books WHERE id = ? ";
        if oracle::host_admits(variant, arith, false).is_ok() {
            let accesses = oracle::sqlite_accesses_for(variant, arith, false);
            assert_eq!(oracle::violation(variant, &accesses), None, "{variant:?}");
            if variant.typed() {
                let lowered = oracle::lowered_for(variant, arith, false).unwrap();
                assert_ne!(
                    lowered,
                    bookclerk_db_exec::lower_canonical_sql(sea_orm::DatabaseBackend::Sqlite, arith),
                    "{variant:?}"
                );
            }
        }
    }
}
