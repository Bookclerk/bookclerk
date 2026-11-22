//! Text vectors for taste matching, stored as f32 LE blobs in the library
//! database.
//!
//! The host loads no model. The embedder here is the stand-in used while no
//! plugin serves text embeddings: a hash of the words of a text, which
//! carries no meaning (`docs/adr/ai-models.md`).

use bookclerk_library::{LibraryStore, WorkRecord};
use sha2::{Digest, Sha256};

use crate::error::{DiscoverError, Result};

/// Model id stored beside hash vectors. They are never compared with a
/// model's vectors.
pub const MODEL_LOCAL_HASH_V1: &str = "local-hash-v1";

/// Numbers in one hash vector.
pub const LOCAL_HASH_DIMENSIONS: usize = 384;

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
/// It carries no meaning: two texts score alike only when they share
/// words. It keeps the whole pipeline (hash → store → cosine) working while
/// no plugin serves text embeddings.
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

/// Opens the embedder the host uses while no plugin serves text embeddings.
///
/// # Returns
///
/// The local-hash embedder, [`LOCAL_HASH_DIMENSIONS`] numbers a vector.
#[must_use]
pub fn open_embedder() -> Box<dyn Embedder> {
    Box::new(HashEmbedder::new(LOCAL_HASH_DIMENSIONS))
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
    fn the_stand_in_embedder_is_local_hash() {
        let embedder = open_embedder();
        assert_eq!(embedder.model_id(), MODEL_LOCAL_HASH_V1);
        assert_eq!(embedder.dimensions(), LOCAL_HASH_DIMENSIONS);
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
