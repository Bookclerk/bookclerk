//! Local text embeddings stored as f32 LE blobs in SQLite.

use std::panic::{self, AssertUnwindSafe};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use bookclerk_library::{LibraryStore, WorkRecord};
use sha2::{Digest, Sha256};

use crate::error::{DiscoverError, Result};

/// Quantized MiniLM-L6-v2 — ~22 MB ONNX, ~50 MB RAM, 384 dims.
pub const MODEL_ALL_MINILM_L6_V2_Q: &str = "all-minilm-l6-v2-q";

/// Local hash embedder id used when ONNX is unavailable / disabled.
pub const MODEL_LOCAL_HASH_V1: &str = "local-hash-v1";

/// Set once the ONNX runtime has panicked while loading in this process.
///
/// `ort` panics instead of returning an error when `libonnxruntime` cannot be
/// loaded, and the panic poisons its global mutex, so every later attempt
/// panics with "Mutex poisoned". [`OnnxEmbedder::open`] loads the library
/// itself first so a missing one is an error, not a panic; this flag is the
/// backstop for any other load panic: after one, [`open_embedder`] goes
/// straight to the hash embedder for the rest of the process.
static ONNX_PANICKED: AtomicBool = AtomicBool::new(false);

/// Resolve the configured model id.
///
/// # Arguments
///
/// * `configured` - Configured model id from config/env; empty selects the build default.
///
/// # Returns
///
/// String result for this operation.
#[must_use]
pub fn embedding_model_id(configured: &str) -> &str {
    let trimmed = configured.trim();
    if trimmed.is_empty() {
        default_embedding_model_id()
    } else {
        trimmed
    }
}

/// Preferred model id for this build (ONNX MiniLM; runtime may fall back).
#[must_use]
pub fn default_embedding_model_id() -> &'static str {
    MODEL_ALL_MINILM_L6_V2_Q
}

/// Produce dense vectors for recommendation / similarity search.
pub trait Embedder: Send {
    /// Stable embedding model id stored alongside vectors in SQLite.
    fn model_id(&self) -> &str;
    /// Length of each embedding vector produced by this backend.
    fn dimensions(&self) -> usize;
    /// Embeds each input text into a dense float vector of [`Self::dimensions`] length.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    fn embed(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>>;
}

/// Deterministic offline embedder for discovery similarity.
///
/// Not semantic like MiniLM — used when ONNX fails to load or when
/// `prefer_onnx` is false. Still enables the full pipeline (hash → store →
/// cosine) for CI and constrained hosts.
#[derive(Debug, Default)]
pub struct HashEmbedder {
    /// Embedding length after clamp to `8..=384`.
    dims: usize,
}

impl HashEmbedder {
    /// Creates a hash embedder with the given vector length (clamped to 8..=384).
    ///
    /// # Arguments
    ///
    /// * `dims` - Desired embedding length before clamping.
    ///
    /// # Returns
    ///
    /// Embedder that maps text to deterministic unit vectors of length `dims`.
    #[must_use]
    pub fn new(dims: usize) -> Self {
        Self {
            dims: dims.clamp(8, 384),
        }
    }
}

impl Embedder for HashEmbedder {
    fn model_id(&self) -> &str {
        MODEL_LOCAL_HASH_V1
    }

    fn dimensions(&self) -> usize {
        self.dims
    }

    fn embed(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|t| hash_embed(t, self.dims)).collect())
    }
}

/// Deterministic token-hash vector (SHA-256 bucket + sign), then L2-normalized.
fn hash_embed(text: &str, dims: usize) -> Vec<f32> {
    let mut out = vec![0.0f32; dims];
    for (i, tok) in text.to_lowercase().split_whitespace().enumerate() {
        let mut hasher = Sha256::new();
        hasher.update(tok.as_bytes());
        let digest = hasher.finalize();
        let idx = u32::from_le_bytes([digest[0], digest[1], digest[2], digest[3]]) as usize % dims;
        let sign = if digest[4] & 1 == 0 { 1.0 } else { -1.0 };
        out[idx] += sign * (1.0 + (i % 7) as f32 * 0.1);
    }
    l2_normalize(&mut out);
    out
}

/// ONNX MiniLM embedder (fastembed). Loaded on demand; drop to free RAM.
pub struct OnnxEmbedder {
    /// Loaded ONNX MiniLM session (dropped to free RAM).
    model: fastembed::TextEmbedding,
    /// Stable id stored beside vectors in SQLite (`all-minilm-l6-v2-q`).
    model_id: String,
    /// Output length of each MiniLM vector (384).
    dims: usize,
}

impl OnnxEmbedder {
    /// Download (if needed) and load quantized MiniLM into `cache_dir`.
    ///
    /// # Arguments
    ///
    /// * `cache_dir` - Directory used to cache downloaded model weights.
    /// * `intra_threads` - ONNX intra-op thread count (clamped to at least 1).
    ///
    /// # Returns
    ///
    /// On success, the inner `Self` value.
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying I/O, parse, network, or store operation fails.
    pub fn open(cache_dir: &Path, intra_threads: usize) -> Result<Self> {
        std::fs::create_dir_all(cache_dir).map_err(|e| DiscoverError::Embed(e.to_string()))?;
        // Load the runtime library first, the way `ort` would: `ort` panics
        // when it cannot, while this returns the error.
        drop(
            ort::init_from(onnx_runtime_library())
                .map_err(|e| DiscoverError::Embed(format!("ONNX Runtime library: {e}")))?,
        );
        let options = fastembed::TextInitOptions::new(fastembed::EmbeddingModel::AllMiniLML6V2Q)
            .with_cache_dir(cache_dir.to_path_buf())
            .with_show_download_progress(true)
            .with_intra_threads(intra_threads.max(1));
        let model = fastembed::TextEmbedding::try_new(options)
            .map_err(|e| DiscoverError::Embed(e.to_string()))?;
        Ok(Self {
            model,
            model_id: MODEL_ALL_MINILM_L6_V2_Q.to_string(),
            dims: 384,
        })
    }
}

/// The ONNX Runtime library `ort` loads: `ORT_DYLIB_PATH` when set, else
/// the platform's library name (found beside the executable or on the
/// loader's search path).
fn onnx_runtime_library() -> std::path::PathBuf {
    match std::env::var_os("ORT_DYLIB_PATH") {
        Some(path) if !path.is_empty() => path.into(),
        _ if cfg!(target_os = "windows") => "onnxruntime.dll".into(),
        _ if cfg!(target_os = "macos") => "libonnxruntime.dylib".into(),
        _ => "libonnxruntime.so".into(),
    }
}

impl Embedder for OnnxEmbedder {
    fn model_id(&self) -> &str {
        &self.model_id
    }

    fn dimensions(&self) -> usize {
        self.dims
    }

    fn embed(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let docs: Vec<&str> = texts.iter().map(String::as_str).collect();
        self.model
            .embed(docs, Some(8))
            .map_err(|e| DiscoverError::Embed(e.to_string()))
    }
}

/// Build the text blob embedded for a work.
///
/// # Arguments
///
/// * `work` - Library work row whose metadata is turned into embedding text.
///
/// # Returns
///
/// String result for this operation.
#[must_use]
pub fn text_for_work(work: &WorkRecord) -> String {
    let mut parts = Vec::new();
    parts.push(work.title.clone());
    if let Some(a) = &work.authors {
        parts.push(a.clone());
    }
    if let Some(n) = &work.narrators {
        parts.push(n.clone());
    }
    if let Some(s) = &work.series {
        parts.push(s.clone());
    }
    if let Some(c) = &work.categories {
        parts.push(c.clone());
    }
    if let Some(s) = &work.subjects {
        parts.push(s.clone());
    }
    if let Some(d) = &work.description {
        let truncated: String = d.chars().take(1200).collect();
        parts.push(truncated);
    }
    parts.join("\n")
}

/// SHA-256 hex of embedding input text.
///
/// # Arguments
///
/// * `text` - Input text to hash or embed.
///
/// # Returns
///
/// String result for this operation.
#[must_use]
pub fn text_hash(text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    hex::encode(hasher.finalize())
}

/// Encode f32 slice as little-endian bytes.
///
/// # Arguments
///
/// * `v` - Float vector to encode.
///
/// # Returns
///
/// Collected results (may be empty).
#[must_use]
pub fn vector_to_bytes(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for f in v {
        out.extend_from_slice(&f.to_le_bytes());
    }
    out
}

/// Decode little-endian f32 blob.
///
/// # Arguments
///
/// * `bytes` - Little-endian f32 blob previously produced by [`vector_to_bytes`].
///
/// # Returns
///
/// Collected results (may be empty).
#[must_use]
pub fn bytes_to_vector(bytes: &[u8]) -> Vec<f32> {
    bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|c| f32::from_le_bytes(*c))
        .collect()
}

/// Scales `v` to unit L2 length in place; a near-zero vector is left unchanged.
fn l2_normalize(v: &mut [f32]) {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 1e-12 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

/// Cosine similarity for equal-length L2-normalized (or raw) vectors.
///
/// # Arguments
///
/// * `a` - Left-hand vector.
/// * `b` - Right-hand vector.
///
/// # Returns
///
/// `f32` result.
#[must_use]
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for i in 0..a.len() {
        dot += a[i] * b[i];
        na += a[i] * a[i];
        nb += b[i] * b[i];
    }
    let denom = na.sqrt() * nb.sqrt();
    if denom < 1e-12 {
        0.0
    } else {
        dot / denom
    }
}

/// One cosine-similarity neighbor from an embedding query.
#[derive(Debug, Clone)]
pub struct CosineHit {
    /// Library entity id of the similar work (`works.id`).
    pub target_id: String,
    /// Ranking score (higher is better); units are algorithm-specific.
    pub score: f32,
}

/// Embed works whose text hash changed (or are missing).
///
/// # Arguments
///
/// * `library` - Open library store used for reads/writes.
/// * `embedder` - Embedding backend that produces dense vectors.
///
/// # Returns
///
/// On success, the inner `usize` value.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
pub async fn embed_dirty_works(
    library: &LibraryStore,
    embedder: &mut dyn Embedder,
) -> Result<usize> {
    let model = embedder.model_id().to_string();
    let works = library.list_works().await?;
    let mut dirty_texts = Vec::new();
    let mut dirty_ids = Vec::new();

    for work in &works {
        let text = text_for_work(work);
        let hash = text_hash(&text);
        let existing = library
            .embedding_text_hash("work", &work.id, &model)
            .await?;
        if existing.as_deref() == Some(hash.as_str()) {
            continue;
        }
        dirty_ids.push((work.id.clone(), hash));
        dirty_texts.push(text);
    }

    if dirty_texts.is_empty() {
        return Ok(0);
    }

    let vectors = embedder.embed(&dirty_texts)?;
    if vectors.len() != dirty_ids.len() {
        return Err(DiscoverError::message(format!(
            "embedder returned {} vectors for {} texts",
            vectors.len(),
            dirty_ids.len()
        )));
    }

    let dims = embedder.dimensions() as i64;
    let mut written = 0usize;
    for ((id, hash), vec) in dirty_ids.into_iter().zip(vectors) {
        library
            .upsert_embedding("work", &id, &model, dims, &vector_to_bytes(&vec), &hash)
            .await?;
        written += 1;
    }
    Ok(written)
}

/// Brute-force cosine search against stored work embeddings.
///
/// # Arguments
///
/// * `library` - Open library store used for reads/writes.
/// * `model` - Embedding model id stored beside vectors in SQLite.
/// * `query` - Query vector or free-text search string.
/// * `exclude` - Entity ids to omit from results.
/// * `limit` - Maximum number of results to return.
///
/// # Returns
///
/// On success, the inner `Vec<CosineHit>` value.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
#[allow(dead_code)]
pub async fn similar_works(
    library: &LibraryStore,
    model: &str,
    query: &[f32],
    exclude: &[String],
    limit: usize,
) -> Result<Vec<CosineHit>> {
    let all = library.list_embeddings("work", model).await?;
    let mut hits = Vec::new();
    for (id, blob) in all {
        if exclude.iter().any(|e| e == &id) {
            continue;
        }
        let v = bytes_to_vector(&blob);
        let score = cosine(query, &v);
        hits.push(CosineHit {
            target_id: id,
            score,
        });
    }
    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    hits.truncate(limit);
    Ok(hits)
}

/// Open an embedder for the current build / config.
///
/// When `prefer_onnx` is true, tries quantized MiniLM via fastembed/ort and
/// **warns + falls back** to the local-hash embedder if load/download fails
/// (missing glibc symbols, offline host, corrupt cache, …).
///
/// # Arguments
///
/// * `models_dir` - Directory that holds (or will hold) embedding model files.
/// * `intra_threads` - ONNX intra-op thread count (clamped to at least 1).
/// * `prefer_onnx` - When true, try MiniLM ONNX first and fall back to hash embeddings on failure.
///
/// # Returns
///
/// On success, the inner `Box<dyn Embedder>` value.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
pub fn open_embedder(
    models_dir: &Path,
    intra_threads: usize,
    prefer_onnx: bool,
) -> Result<Box<dyn Embedder>> {
    open_embedder_with(&ONNX_PANICKED, models_dir, intra_threads, prefer_onnx)
}

/// [`open_embedder`] with the "runtime panicked" flag passed in, so tests can
/// use their own flag instead of the process-wide one.
///
/// # Arguments
///
/// * `onnx_panicked` - Set when an ONNX load panicked; when set, ONNX is skipped.
/// * `models_dir` - Directory that holds (or will hold) embedding model files.
/// * `intra_threads` - ONNX intra-op thread count (clamped to at least 1).
/// * `prefer_onnx` - When true, try MiniLM ONNX first and fall back to hash embeddings on failure.
///
/// # Returns
///
/// The ONNX embedder when it loads, otherwise the local-hash embedder.
///
/// # Errors
///
/// Currently never fails; the `Result` matches [`open_embedder`].
fn open_embedder_with(
    onnx_panicked: &AtomicBool,
    models_dir: &Path,
    intra_threads: usize,
    prefer_onnx: bool,
) -> Result<Box<dyn Embedder>> {
    if prefer_onnx && !onnx_panicked.load(Ordering::Acquire) {
        // The model and cache path are owned by the closure's caller and are
        // not reused after a panic, so asserting unwind safety is sound.
        match panic::catch_unwind(AssertUnwindSafe(|| {
            OnnxEmbedder::open(models_dir, intra_threads)
        })) {
            Ok(Ok(e)) => return Ok(Box::new(e)),
            Ok(Err(err)) => {
                tracing::warn!(
                    error = %err,
                    "ONNX MiniLM embedder unavailable; falling back to local-hash embeddings"
                );
            }
            Err(payload) => {
                onnx_panicked.store(true, Ordering::Release);
                tracing::warn!(
                    error = panic_message(payload.as_ref()),
                    "ONNX runtime failed to load; using local-hash embeddings until restart"
                );
            }
        }
    }
    Ok(Box::new(HashEmbedder::new(384)))
}

/// Text of a caught panic payload, for the fallback warning.
///
/// # Arguments
///
/// * `payload` - Payload returned by [`panic::catch_unwind`].
///
/// # Returns
///
/// The panic message when it was a string, otherwise a fixed placeholder.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> &str {
    if let Some(s) = payload.downcast_ref::<&str>() {
        s
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s
    } else {
        "non-string panic payload"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_embed_is_stable() {
        let mut e = HashEmbedder::new(32);
        let a = e.embed(&[String::from("The Hobbit Tolkien")]).unwrap();
        let b = e.embed(&[String::from("The Hobbit Tolkien")]).unwrap();
        assert_eq!(a, b);
        assert!((cosine(&a[0], &b[0]) - 1.0).abs() < 1e-5);
    }

    #[test]
    fn vector_roundtrip() {
        let v = vec![0.1f32, -0.2, 0.3];
        let back = bytes_to_vector(&vector_to_bytes(&v));
        assert_eq!(v, back);
    }

    #[test]
    fn panic_message_reads_string_payloads() {
        let caught = panic::catch_unwind(|| panic!("Mutex poisoned")).unwrap_err();
        assert_eq!(panic_message(caught.as_ref()), "Mutex poisoned");
        let owned = panic::catch_unwind(|| panic!("{}", String::from("dylib"))).unwrap_err();
        assert_eq!(panic_message(owned.as_ref()), "dylib");
    }

    #[test]
    fn a_missing_onnx_runtime_is_an_error_not_a_panic() {
        // Only this test sets the variable; no other test loads ONNX.
        std::env::set_var("ORT_DYLIB_PATH", "/nonexistent/libonnxruntime.so");
        let dir = std::env::temp_dir().join("bookclerk-discover-onnx-missing");
        let opened = panic::catch_unwind(|| OnnxEmbedder::open(&dir, 1));
        std::env::remove_var("ORT_DYLIB_PATH");
        assert!(matches!(opened, Ok(Err(DiscoverError::Embed(_)))));
    }

    #[test]
    fn open_embedder_skips_onnx_after_a_runtime_panic() {
        let panicked = AtomicBool::new(true);
        let dir = std::env::temp_dir().join("bookclerk-discover-onnx-panicked");
        let embedder = open_embedder_with(&panicked, &dir, 1, true).unwrap();
        assert_eq!(embedder.model_id(), MODEL_LOCAL_HASH_V1);
    }
}
