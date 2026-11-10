//! The PostgreSQL adapter passes the shared adapter conformance suite
//! (`bookclerk_db_testkit::adapter`) as the shipped guest binary, serving a
//! fresh binding database over the Cap'n Proto boundary the host speaks.

// Test code: unwrap/expect are fine here.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;

use bookclerk_db_testkit::adapter::NativeAdapter;
use bookclerk_db_testkit::run_adapter_suite;
use bookclerk_plugin_abi::db::{binding_values_from_params, DbConnectParams};

/// The disposable server's admin URL.
fn postgres_test_url() -> String {
    let url = std::env::var("BOOKCLERK_TEST_POSTGRES_URL")
        .expect("BOOKCLERK_TEST_POSTGRES_URL is required to run the postgres adapter suite");
    assert!(
        !url.trim().is_empty(),
        "BOOKCLERK_TEST_POSTGRES_URL must not be empty"
    );
    url
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires BOOKCLERK_TEST_POSTGRES_URL and a disposable Postgres"]
async fn postgres_shipped_guest_conforms_over_rpc() {
    let url = postgres_test_url();
    let tmp = tempfile::tempdir().expect("tmpdir");
    // A name of its own per run, so the suite always starts on an empty unit.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after 1970")
        .subsec_nanos();
    let database = format!("bookclerk_conformance_{}_{nanos}", std::process::id());
    let values = binding_values_from_params(&DbConnectParams::Postgres {
        plugin_data_dir: tmp.path().to_string_lossy().into_owned(),
        url,
        binding: Some("conformance".into()),
        database: Some(database.clone()),
        provision: true,
    })
    .expect("binding values");
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let guest = NativeAdapter::spawn(
                Path::new(env!("CARGO_BIN_EXE_bookclerk-plugin-database-postgres")),
                &[
                    ("HOME", tmp.path().as_os_str()),
                    ("TMPDIR", tmp.path().as_os_str()),
                ],
                values,
            )
            .await
            .expect("spawn the postgres guest");
            let report = run_adapter_suite(guest.database()).await;
            // Drop the unit before judging, so a failed run leaves nothing.
            let dropped = guest.database().drop_unit(&database).await;
            guest.shutdown().await;
            assert!(
                report.passed(),
                "postgres adapter breaks the contract:\n{report}"
            );
            dropped.expect("drop the conformance database");
        })
        .await;
}
