//! External local filesystem output plugin for Bookclerk.

use bookclerk_plugin_destination_local::plugin::LocalRoot;
use bookclerk_plugin_sdk::serve;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    serve(LocalRoot).await?;
    Ok(())
}
