//! Adapter-boundary typed conformance for the platform SQLite guest.

use bookclerk_db_guest::{guest_execute_request, set_connection};
use bookclerk_plugin_abi::DbCapabilities;

#[tokio::test]
async fn platform_sqlite_guest_execute_passes_typed_vectors() {
    let db = bookclerk_plugin_database_sqlite::open_memory()
        .await
        .expect("mem db");
    set_connection(db).await;
    bookclerk_library::sql_plan::run_typed_request_vectors(
        DbCapabilities::advertised_sqlite(),
        DbCapabilities::advertised_sqlite().max_result_rows,
        |req| async move {
            guest_execute_request(req)
                .await
                .map_err(|err| err.to_string())
        },
    )
    .await;
}
