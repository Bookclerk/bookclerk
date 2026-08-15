//! External S3 / MinIO output plugin for Bookclerk (ABI v2).

use bookclerk_plugin_destination_s3::v2::S3Root;
use bookclerk_plugin_sdk::serve_v2;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    serve_v2(S3Root).await?;
    Ok(())
}
