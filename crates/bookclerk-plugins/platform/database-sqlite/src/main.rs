//! Platform SQLite database plugin guest (ABI v2).

use bookclerk_plugin_database_sqlite::v2::SqliteRoot;
use bookclerk_plugin_sdk::serve_v2;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    serve_v2(SqliteRoot).await?;
    Ok(())
}
