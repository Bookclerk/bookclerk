//! External S3 / MinIO output plugin for Bookclerk.

use bookclerk_plugin_destination_s3::plugin::S3Root;
use bookclerk_plugin_sdk::serve;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    serve(S3Root).await?;
    Ok(())
}
