//! External Cloudflare R2 output plugin for Bookclerk.

use bookclerk_plugin_destination_r2::plugin::R2Root;
use bookclerk_plugin_sdk::serve;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    serve(R2Root).await?;
    Ok(())
}
