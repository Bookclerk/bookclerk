//! Text-embedding model plugin.
//!
//! The platform `embedder` guest serves text-embedding models on the `ai`
//! entrypoint (`docs/adr/ai-models.md`). It has no network access and no
//! settings: it serves every model directory it finds under `models/` in
//! its data directory, with the language packs present there. The host
//! puts verified model files in that directory and removes them.
//!
//! A run goes through three modules:
//!
//! 1. [`text`] turns a text into the model's token ids with the whole
//!    vocabulary (`bookclerk-sentencepiece`).
//! 2. [`model`] looks each token's table rows up in the row packs on disk
//!    (`bookclerk-model-pack`) and spells a token whose row is missing with
//!    smaller pieces.
//! 3. [`engine`] runs the cut ONNX graph on those rows with the pinned ONNX
//!    Runtime shared library that sits beside the plugin's executable.
//!
//! The layout of a model directory is described in
//! `docs/internals/model-packs.md`.

pub mod engine;
pub mod model;
pub mod plugin;
pub mod text;

/// Plugin id advertised in `describe()`; models are named `@embedder/<model id>`.
pub const ID: &str = "embedder";

/// Directory under the plugin's data directory that holds one directory
/// per model.
pub const MODELS_DIR: &str = "models";

/// Name of the ONNX Runtime shared library beside the plugin executable on
/// this platform (the same name `bookclerk-onnxruntime` installs).
pub const RUNTIME_LIBRARY: &str = if cfg!(target_os = "windows") {
    "onnxruntime.dll"
} else if cfg!(target_os = "macos") {
    "libonnxruntime.dylib"
} else {
    "libonnxruntime.so"
};
