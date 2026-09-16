//! Platform SQLite database plugin guest.

use bookclerk_plugin_database_sqlite::plugin::SqliteRoot;
use bookclerk_plugin_sdk::serve;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    serve(SqliteRoot).await?;
    Ok(())
}
