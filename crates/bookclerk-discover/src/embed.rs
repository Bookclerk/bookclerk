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

/// Set once loading ONNX has failed or panicked in this process.
///
/// A failed load can take many seconds (a missing runtime library, a model
/// download that times out), so it is attempted once per process: after a
/// failure, [`open_embedder`] goes straight to the hash embedder until the
/// process restarts, and no later request waits on ONNX again. A panic is
/// covered too: `ort` panics instead of returning an error when
/// `libonnxruntime` cannot be loaded, and the panic poisons its global mutex,
/// so every later attempt would panic with "Mutex poisoned".
static ONNX_UNAVAILABLE: AtomicBool = AtomicBool::new(false);

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
///
/// Only built with the `onnx` feature (the hosts' `onnx-embeddings`).
#[cfg(feature = "onnx")]
pub struct OnnxEmbedder {
    /// Loaded ONNX MiniLM session (dropped to free RAM).
    model: fastembed::TextEmbedding,
    /// Stable id stored beside vectors in SQLite (`all-minilm-l6-v2-q`).
    model_id: String,
    /// Output length of each MiniLM vector (384).
    dims: usize,
}

#[cfg(feature = "onnx")]
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
#[cfg(feature = "onnx")]
fn onnx_runtime_library() -> std::path::PathBuf {
    match std::env::var_os("ORT_DYLIB_PATH") {
        Some(path) if !path.is_empty() => path.into(),
        _ if cfg!(target_os = "windows") => "onnxruntime.dll".into(),
        _ if cfg!(target_os = "macos") => "libonnxruntime.dylib".into(),
        _ => "libonnxruntime.so".into(),
    }
}

#[cfg(feature = "onnx")]
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
    if !work.credits.is_empty() {
        parts.push(work.credits.join(", "));
    }
    if !work.narrators.is_empty() {
        parts.push(work.narrators.join(", "));
    }
    if !work.series.is_empty() {
        parts.push(work.series.join(", "));
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

/// Works read per page by [`embed_dirty_works`]; also the most texts one
/// embedder call receives.
pub const EMBED_PAGE_WORKS: u64 = 128;

/// Embed works whose text hash changed (or are missing), and drop the
/// embeddings of works that no longer exist.
///
/// Walks `works` in keyset pages of [`EMBED_PAGE_WORKS`]: each page costs
/// one read of the works, one chunked read of their stored hashes, and one
/// embedder call for the dirty ones, so memory stays bounded by the page
/// and the store sees no per-work lookup.
///
/// # Arguments
///
/// * `library` - Open library store used for reads/writes.
/// * `embedder` - Embedding backend that produces dense vectors.
///
/// # Returns
///
/// How many embeddings were written.
///
/// # Errors
///
/// Returns an error when a store read or write fails, or when the
/// embedder fails or returns a different number of vectors than texts.
pub async fn embed_dirty_works(
    library: &LibraryStore,
    embedder: &mut dyn Embedder,
) -> Result<usize> {
    let model = embedder.model_id().to_string();
    let dims = i64::try_from(embedder.dimensions())
        .map_err(|_| DiscoverError::message("embedder dimensions exceed i64"))?;
    library.prune_orphan_work_embeddings().await?;
    let mut written = 0usize;
    let mut after: Option<String> = None;
    loop {
        let works = library
            .list_works_page(after.as_deref(), EMBED_PAGE_WORKS)
            .await?;
        let Some(last) = works.last() else {
            break;
        };
        after = Some(last.id.clone());
        let ids: Vec<String> = works.iter().map(|work| work.id.clone()).collect();
        let stored = library.embedding_text_hashes("work", &model, &ids).await?;
        let mut dirty_texts = Vec::new();
        let mut dirty_ids = Vec::new();
        for work in &works {
            let text = text_for_work(work);
            let hash = text_hash(&text);
            if stored.get(&work.id) == Some(&hash) {
                continue;
            }
            dirty_ids.push((work.id.clone(), hash));
            dirty_texts.push(text);
        }
        if !dirty_texts.is_empty() {
            let vectors = embedder.embed(&dirty_texts)?;
            if vectors.len() != dirty_ids.len() {
                return Err(DiscoverError::message(format!(
                    "embedder returned {} vectors for {} texts",
                    vectors.len(),
                    dirty_ids.len()
                )));
            }
            for ((id, hash), vec) in dirty_ids.into_iter().zip(vectors) {
                library
                    .upsert_embedding("work", &id, &model, dims, &vector_to_bytes(&vec), &hash)
                    .await?;
                written += 1;
            }
        }
        if works.len() < usize::try_from(EMBED_PAGE_WORKS).unwrap_or(usize::MAX) {
            break;
        }
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
    open_embedder_with(&ONNX_UNAVAILABLE, models_dir, intra_threads, prefer_onnx)
}

/// [`open_embedder`] with the "ONNX unavailable" flag passed in, so tests can
/// use their own flag instead of the process-wide one.
///
/// # Arguments
///
/// * `onnx_unavailable` - Set once an ONNX load failed or panicked; when set, ONNX is skipped.
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
    onnx_unavailable: &AtomicBool,
    models_dir: &Path,
    intra_threads: usize,
    prefer_onnx: bool,
) -> Result<Box<dyn Embedder>> {
    if prefer_onnx && !onnx_unavailable.load(Ordering::Acquire) {
        // The model and cache path are owned by the closure's caller and are
        // not reused after a panic, so asserting unwind safety is sound.
        match panic::catch_unwind(AssertUnwindSafe(|| open_onnx(models_dir, intra_threads))) {
            Ok(Ok(e)) => return Ok(e),
            Ok(Err(err)) => {
                onnx_unavailable.store(true, Ordering::Release);
                tracing::warn!(
                    error = %err,
                    "ONNX MiniLM embedder unavailable; using local-hash embeddings until restart"
                );
            }
            Err(payload) => {
                onnx_unavailable.store(true, Ordering::Release);
                tracing::warn!(
                    error = panic_message(payload.as_ref()),
                    "ONNX runtime failed to load; using local-hash embeddings until restart"
                );
            }
        }
    }
    Ok(Box::new(HashEmbedder::new(384)))
}

/// Loads the ONNX MiniLM embedder.
///
/// # Arguments
///
/// * `models_dir` - Directory that holds (or will hold) embedding model files.
/// * `intra_threads` - ONNX intra-op thread count (clamped to at least 1).
///
/// # Returns
///
/// The loaded embedder.
///
/// # Errors
///
/// Returns [`DiscoverError::Embed`] when the runtime library or the model
/// cannot be loaded.
#[cfg(feature = "onnx")]
fn open_onnx(models_dir: &Path, intra_threads: usize) -> Result<Box<dyn Embedder>> {
    Ok(Box::new(OnnxEmbedder::open(models_dir, intra_threads)?))
}

/// Stand-in for the ONNX loader in a build without the `onnx` feature.
///
/// # Arguments
///
/// * `_models_dir` - Unused; the signature matches the ONNX build's loader.
/// * `_intra_threads` - Unused; the signature matches the ONNX build's loader.
///
/// # Returns
///
/// Never returns an embedder.
///
/// # Errors
///
/// Always returns [`DiscoverError::Embed`] naming the missing feature, so
/// [`open_embedder`] warns once and uses the local-hash embedder.
#[cfg(not(feature = "onnx"))]
fn open_onnx(_models_dir: &Path, _intra_threads: usize) -> Result<Box<dyn Embedder>> {
    Err(DiscoverError::Embed(String::from(
        "this build has no ONNX embedder (built without the `onnx-embeddings` feature)",
    )))
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

    /// A work with only the given credits, narrators and series lists filled.
    fn work(credits: &[&str], narrators: &[&str], series: &[&str]) -> WorkRecord {
        let now = chrono::Utc::now();
        WorkRecord {
            id: "w1".into(),
            kind: "book".into(),
            title: "The Hobbit".into(),
            parent_work_id: None,
            credits: credits.iter().map(|s| (*s).to_string()).collect(),
            narrators: narrators.iter().map(|s| (*s).to_string()).collect(),
            series: series.iter().map(|s| (*s).to_string()).collect(),
            description: None,
            subjects: None,
            categories: None,
            cover_url: None,
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn text_for_work_joins_each_present_field_on_its_own_line() {
        let mut full = work(
            &["J. R. R. Tolkien", "Christopher Tolkien"],
            &["Andy Serkis"],
            &["Middle-earth"],
        );
        full.categories = Some("Fantasy".into());
        full.subjects = Some("Dragons".into());
        full.description = Some("x".repeat(1300));
        let expected = format!(
            "The Hobbit\nJ. R. R. Tolkien, Christopher Tolkien\nAndy Serkis\nMiddle-earth\nFantasy\nDragons\n{}",
            "x".repeat(1200)
        );
        assert_eq!(text_for_work(&full), expected);
    }

    #[test]
    fn text_for_work_leaves_out_empty_lists() {
        assert_eq!(text_for_work(&work(&[], &[], &[])), "The Hobbit");
        assert_eq!(text_for_work(&work(&["A"], &[], &[])), "The Hobbit\nA");
        assert_eq!(text_for_work(&work(&[], &["N"], &[])), "The Hobbit\nN");
        assert_eq!(text_for_work(&work(&[], &[], &["S"])), "The Hobbit\nS");
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

    #[cfg(feature = "onnx")]
    #[test]
    fn a_missing_onnx_runtime_is_an_error_not_a_panic() {
        // Only this test sets the variable; no other test loads ONNX.
        std::env::set_var("ORT_DYLIB_PATH", "/nonexistent/libonnxruntime.so");
        let dir = std::env::temp_dir().join("bookclerk-discover-onnx-missing");
        let opened = panic::catch_unwind(|| open_onnx(&dir, 1));
        std::env::remove_var("ORT_DYLIB_PATH");
        assert!(matches!(opened, Ok(Err(DiscoverError::Embed(_)))));
    }

    #[cfg(not(feature = "onnx"))]
    #[test]
    fn a_build_without_onnx_falls_back_to_local_hash_and_remembers_it() {
        let unavailable = AtomicBool::new(false);
        let dir = std::env::temp_dir().join("bookclerk-discover-onnx-not-built");
        let embedder = open_embedder_with(&unavailable, &dir, 1, true).unwrap();
        assert_eq!(embedder.model_id(), MODEL_LOCAL_HASH_V1);
        assert!(unavailable.load(Ordering::Acquire));
        assert!(matches!(open_onnx(&dir, 1), Err(DiscoverError::Embed(_))));
    }

    #[test]
    fn open_embedder_skips_onnx_after_a_failed_load() {
        let unavailable = AtomicBool::new(true);
        let dir = std::env::temp_dir().join("bookclerk-discover-onnx-panicked");
        let embedder = open_embedder_with(&unavailable, &dir, 1, true).unwrap();
        assert_eq!(embedder.model_id(), MODEL_LOCAL_HASH_V1);
    }

    use bookclerk_library::NewBook;

    /// Wraps [`HashEmbedder`] and records the size of every `embed` call.
    struct CountingEmbedder {
        /// The real embedder.
        inner: HashEmbedder,
        /// Texts per call, in call order.
        calls: Vec<usize>,
    }

    impl Embedder for CountingEmbedder {
        fn model_id(&self) -> &str {
            self.inner.model_id()
        }

        fn dimensions(&self) -> usize {
            self.inner.dimensions()
        }

        fn embed(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
            self.calls.push(texts.len());
            self.inner.embed(texts)
        }
    }

    #[tokio::test]
    async fn embeds_each_page_once_skips_clean_works_and_drops_orphans() {
        let store = bookclerk_plugin_database_sqlite::open_store_memory()
            .await
            .unwrap();
        store
            .upsert_account("audible", "acct", "us", None, true, "audible")
            .await
            .unwrap();
        // One page and a bit, so the walk crosses a page boundary.
        let total = usize::try_from(EMBED_PAGE_WORKS).unwrap() + 3;
        for i in 0..total {
            let mut book = NewBook::minimal(format!("B{i:09}"), "acct", "us", format!("Title {i}"));
            book.authors = Some(format!("Author {i}"));
            store.upsert_book(&book).await.unwrap();
        }
        let works = store.list_works().await.unwrap().len();
        assert_eq!(works, total);

        let mut embedder = CountingEmbedder {
            inner: HashEmbedder::new(16),
            calls: Vec::new(),
        };
        let model = embedder.model_id().to_string();
        // A vector left behind by a work that no longer exists.
        store
            .upsert_embedding("work", "deleted-work", &model, 16, &[0; 64], "stale")
            .await
            .unwrap();

        assert_eq!(
            embed_dirty_works(&store, &mut embedder).await.unwrap(),
            works
        );
        assert_eq!(
            embedder.calls,
            [usize::try_from(EMBED_PAGE_WORKS).unwrap(), 3]
        );
        let stored = store.list_embeddings("work", &model).await.unwrap();
        assert_eq!(stored.len(), works);
        assert!(stored.iter().all(|(id, _)| id != "deleted-work"));

        // Nothing changed: no embedder call, nothing written.
        embedder.calls.clear();
        assert_eq!(embed_dirty_works(&store, &mut embedder).await.unwrap(), 0);
        assert!(embedder.calls.is_empty());
    }
}
