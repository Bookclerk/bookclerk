//! External local filesystem output plugin for Bookclerk (ABI v2).

use bookclerk_plugin_destination_local::v2::LocalRoot;
use bookclerk_plugin_sdk::serve_v2;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    serve_v2(LocalRoot).await?;
    Ok(())
}
