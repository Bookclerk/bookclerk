//! Text-embedding model plugin for Bookclerk.

use bookclerk_plugin_ai_embedder::plugin::EmbedderRoot;
use bookclerk_plugin_sdk::serve;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    serve(EmbedderRoot::from_environment()).await?;
    Ok(())
}
