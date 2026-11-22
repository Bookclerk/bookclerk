//! External Backblaze B2 output plugin for Bookclerk.

use bookclerk_plugin_destination_b2::plugin::B2Root;
use bookclerk_plugin_sdk::serve;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    serve(B2Root).await?;
    Ok(())
}
