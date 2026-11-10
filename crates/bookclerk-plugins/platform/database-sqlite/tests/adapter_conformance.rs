//! The platform SQLite adapter passes the shared adapter conformance suite
//! (`bookclerk_db_testkit::adapter`) in process and as the shipped guest
//! binary over the Cap'n Proto boundary the host speaks.

// Test code: unwrap/expect are fine here.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;

use bookclerk_db_testkit::adapter::NativeAdapter;
use bookclerk_db_testkit::assert_adapter_conforms;
use bookclerk_plugin_abi::db::{binding_values_from_params, DbConnectParams};

/// A dedicated binding connection on an empty in-memory unit.
#[tokio::test]
async fn the_binding_adapter_conforms_in_process() {
    let conn = bookclerk_plugin_database_sqlite::open_memory_unmigrated()
        .await
        .expect("empty in-memory unit");
    let database = bookclerk_plugin_database_sqlite::plugin::binding_database(conn);
    let report = assert_adapter_conforms(database.as_ref()).await;
    assert!(
        report
            .capabilities
            .as_ref()
            .is_some_and(|caps| caps.streams_results()),
        "first-party sqlite advertises a streamed budget:\n{report}"
    );
}

/// The shipped guest serving a binding open on an empty unit, reached
/// through the RPC client the host holds for a database plugin.
#[tokio::test(flavor = "current_thread")]
async fn the_shipped_guest_conforms_over_rpc() {
    let tmp = tempfile::tempdir().expect("tmpdir");
    let unit = tmp.path().join("conformance.sqlite");
    let values = binding_values_from_params(&DbConnectParams::Sqlite {
        plugin_data_dir: tmp.path().to_string_lossy().into_owned(),
        sqlite_path: Some(unit.to_string_lossy().into_owned()),
        binding: Some("conformance".into()),
        provision: true,
    })
    .expect("binding values");
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let guest = NativeAdapter::spawn(
                Path::new(env!("CARGO_BIN_EXE_bookclerk-plugin-database-sqlite")),
                &[
                    ("HOME", tmp.path().as_os_str()),
                    ("TMPDIR", tmp.path().as_os_str()),
                ],
                values,
            )
            .await
            .expect("spawn the sqlite guest");
            assert_adapter_conforms(guest.database()).await;
            guest.shutdown().await;
        })
        .await;
}
