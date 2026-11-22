//! Tokenizes texts with a model directory's tokenizer, outside the plugin host.
//!
//! The pack builder's checks use this to compare the plugin's token ids
//! with the published tokenizer's:
//!
//! ```text
//! cargo run -p bookclerk-plugin-ai-embedder --example tokens -- \
//!     <models directory> <model id> < texts.json
//! ```
//!
//! Standard input is a JSON array of `{"text": "...", "query": false}`
//! objects. Standard output is one JSON object with one array of token
//! ids per text under `ids`, framed and cut to the model's token limit as
//! the plugin passes them on.

use std::io::Read;
use std::path::PathBuf;

use bookclerk_model_pack::{PackManifest, MANIFEST_FILE};
use bookclerk_plugin_ai_embedder::text::{Role, TextEncoder};

/// One text of the input.
#[derive(serde::Deserialize)]
struct Text {
    /// The text to tokenize.
    text: String,
    /// True to tokenize it as a search query, false as a document.
    #[serde(default)]
    query: bool,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let (Some(models_dir), Some(model)) = (args.next(), args.next()) else {
        return Err("usage: tokens <models directory> <model id>".into());
    };
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    let texts: Vec<Text> = serde_json::from_str(&input)?;

    let directory = PathBuf::from(models_dir).join(model);
    let manifest = PackManifest::parse(&std::fs::read(directory.join(MANIFEST_FILE))?)?;
    let tokenizer = std::fs::read(directory.join(manifest.tokenizer.model.file.as_str()))?;
    let encoder = TextEncoder::new(tokenizer, &manifest.tokenizer, manifest.max_tokens)?;
    let mut ids = Vec::with_capacity(texts.len());
    for text in texts {
        let role = if text.query {
            Role::Query
        } else {
            Role::Document
        };
        ids.push(encoder.frame(encoder.body_ids(&text.text, role)?));
    }
    println!("{}", serde_json::json!({ "ids": ids }));
    Ok(())
}
