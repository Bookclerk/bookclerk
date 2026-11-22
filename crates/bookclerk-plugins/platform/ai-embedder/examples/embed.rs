//! Embeds texts with a model directory on disk, outside the plugin host.
//!
//! The pack builder's checks use this to compare the plugin's vectors with
//! the published model's:
//!
//! ```text
//! cargo run -p bookclerk-plugin-ai-embedder --example embed -- \
//!     <models directory> <ONNX Runtime library> <model id> < texts.json
//! ```
//!
//! Standard input is a JSON array of `{"text": "...", "query": false}`
//! objects. Standard output is one JSON object with the model's
//! `revision`, the `fallbacks` indexes and one array of numbers per text
//! under `vectors`.

use std::io::Read;
use std::path::PathBuf;

use bookclerk_plugin_ai_embedder::plugin::EmbedderRoot;
use bookclerk_plugin_sdk::ai::embedding_vectors;
use bookclerk_plugin_sdk::{
    AiEmbedText, AiInput, AiRunParams, AiTextEmbeddingsInput, AiTextRole, BindingValues, Bindings,
    Invocation, PluginWorker,
};

/// One text of the input.
#[derive(serde::Deserialize)]
struct Text {
    /// The text to embed.
    text: String,
    /// True to embed it as a search query, false as a document.
    #[serde(default)]
    query: bool,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let (Some(models_dir), Some(library), Some(model)) = (args.next(), args.next(), args.next())
    else {
        return Err("usage: embed <models directory> <ONNX Runtime library> <model id>".into());
    };
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    let texts: Vec<Text> = serde_json::from_str(&input)?;

    let root = EmbedderRoot::new(
        Some(PathBuf::from(models_dir)),
        Some(PathBuf::from(library)),
    );
    let ai = root
        .open(
            Invocation::default(),
            Bindings::from_values(BindingValues::default()),
        )
        .await?
        .ai
        .ok_or("the plugin opened no ai entrypoint")?;
    let mut vectors = Vec::new();
    let mut fallbacks = Vec::new();
    let mut revision = String::new();
    // One text a run keeps each request far under the entrypoint's caps.
    for (index, text) in texts.into_iter().enumerate() {
        let reply = ai
            .run(AiRunParams {
                model: model.clone(),
                input: AiInput {
                    text_embeddings: AiTextEmbeddingsInput {
                        texts: vec![AiEmbedText {
                            text: text.text,
                            role: if text.query {
                                AiTextRole::Query
                            } else {
                                AiTextRole::Document
                            },
                        }],
                    },
                },
            })
            .await?;
        let output = reply.output.text_embeddings;
        if !output.fallbacks.is_empty() {
            fallbacks.push(index);
        }
        vectors.extend(embedding_vectors(&output));
        revision = output.revision;
    }
    println!(
        "{}",
        serde_json::json!({ "revision": revision, "fallbacks": fallbacks, "vectors": vectors })
    );
    Ok(())
}
