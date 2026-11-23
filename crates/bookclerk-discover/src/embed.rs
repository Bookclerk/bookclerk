//! Text vectors for taste matching, stored as f32 LE blobs in the library
//! database.
//!
//! The host loads no model. An [`Embedder`] turns texts into vectors: the
//! daemon's is the model the library chose for text embeddings, run in its
//! plugin, and [`HashEmbedder`] is the stand-in used while no plugin serves
//! one. Every vector is stored with the [`VectorSpace`] that made it, and
//! vectors of two spaces are never compared (`docs/adr/ai-models.md`).
//!
//! Works are embedded by [`embed_dirty_works`]. Everything else a ranking
//! compares (storefront candidates, wishlist entries) is embedded by its
//! text ([`text_vectors`]): one vector per text and space, shared by
//! everyone who ranks that text. [`serving_space`] picks the space a
//! ranking reads, so a library that changed its model keeps ranking with
//! the old vectors until the new model has embedded every work.

use std::collections::HashMap;
use std::time::Instant;

use async_trait::async_trait;
use bookclerk_library::{
    LibraryStore, NewVector, VectorSpace, WorkRecord, VECTOR_KIND_TEXT, VECTOR_KIND_WORK,
};
use sha2::{Digest, Sha256};

use crate::error::{DiscoverError, Result};

/// Model id stored beside hash vectors. They are never compared with a
/// model's vectors.
pub const MODEL_LOCAL_HASH_V1: &str = "local-hash-v1";

/// Numbers in one hash vector.
pub const LOCAL_HASH_DIMENSIONS: usize = 384;

/// Most characters of a description that go into an embedded text. A
/// model reads a bounded number of tokens, and the start of a description
/// says what the book is.
const EMBED_DESCRIPTION_CHARS: usize = 1200;

/// How long a text vector may go unused before it is pruned.
pub const TEXT_VECTOR_MAX_AGE: chrono::Duration = chrono::Duration::days(14);

/// How stale a text vector's last use may get before a ranking that reads
/// it writes the use down again.
const TEXT_VECTOR_TOUCH_AFTER: chrono::Duration = chrono::Duration::days(1);

/// How long the vectors of a space the library left are kept after their
/// last write, so a node still on that space does not lose its work.
pub const OLD_SPACE_MAX_AGE: chrono::Duration = chrono::Duration::days(1);

/// The stand-in's vector space.
#[must_use]
pub fn stand_in_space() -> VectorSpace {
    VectorSpace {
        model: String::from(MODEL_LOCAL_HASH_V1),
        revision: String::new(),
        packs: String::new(),
    }
}

/// Whether `space` is the stand-in's: hash vectors that carry no meaning.
#[must_use]
pub fn is_stand_in(space: &VectorSpace) -> bool {
    space.model == MODEL_LOCAL_HASH_V1
}

/// Produce dense vectors for recommendation / similarity search.
#[async_trait]
pub trait Embedder: Send {
    /// The space of the vectors this embedder makes.
    fn space(&self) -> &VectorSpace;
    /// Length of each embedding vector produced by this backend.
    fn dimensions(&self) -> usize;
    /// Most texts one [`Self::embed`] call takes.
    fn max_batch(&self) -> usize {
        usize::try_from(EMBED_PAGE_WORKS).unwrap_or(usize::MAX)
    }
    /// Embeds each input text into a dense float vector of [`Self::dimensions`] length.
    ///
    /// # Errors
    ///
    /// Returns an error when the model cannot be reached or fails, or when
    /// it now makes vectors of another space than [`Self::space`].
    async fn embed(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>>;
}

/// Deterministic offline embedder for discovery similarity.
///
/// It carries no meaning: two texts score alike only when they share
/// words. It keeps the whole pipeline (hash → store → cosine) working while
/// no plugin serves text embeddings.
#[derive(Debug)]
pub struct HashEmbedder {
    /// Embedding length after clamp to `8..=384`.
    dims: usize,
    /// The stand-in's space.
    space: VectorSpace,
}

impl Default for HashEmbedder {
    fn default() -> Self {
        Self::new(LOCAL_HASH_DIMENSIONS)
    }
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
            space: stand_in_space(),
        }
    }
}

#[async_trait]
impl Embedder for HashEmbedder {
    fn space(&self) -> &VectorSpace {
        &self.space
    }

    fn dimensions(&self) -> usize {
        self.dims
    }

    async fn embed(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
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

/// What is known of a book when its text is embedded. Works and storefront
/// candidates are embedded from the same fields in the same order, so a
/// candidate is compared with works on equal terms.
#[derive(Debug, Clone, Copy, Default)]
pub struct EmbedFields<'a> {
    /// Display title.
    pub title: &'a str,
    /// Authors, as one comma-separated line.
    pub authors: Option<&'a str>,
    /// Series names, as one comma-separated line.
    pub series: Option<&'a str>,
    /// Genres, categories or subjects, as one line.
    pub genres: Option<&'a str>,
    /// Synopsis; only its first 1,200 characters are
    /// used.
    pub description: Option<&'a str>,
}

/// The text embedded for a book: each known field on its own line.
///
/// Narrators are left out: who reads a book says little about what it is,
/// and Discover scores a liked narrator on its own.
#[must_use]
pub fn embed_text(fields: &EmbedFields<'_>) -> String {
    let present = |field: Option<&str>| {
        field
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
    };
    let mut parts = vec![fields.title.trim().to_string()];
    parts.extend(present(fields.authors));
    parts.extend(present(fields.series));
    parts.extend(present(fields.genres));
    if let Some(description) = present(fields.description) {
        parts.push(description.chars().take(EMBED_DESCRIPTION_CHARS).collect());
    }
    parts.join("\n")
}

/// Build the text blob embedded for a work.
///
/// # Arguments
///
/// * `work` - Library work row whose metadata is turned into embedding text.
///
/// # Returns
///
/// The work's [`embed_text`].
#[must_use]
pub fn text_for_work(work: &WorkRecord) -> String {
    let authors = work.credits.join(", ");
    let series = work.series.join(", ");
    let genres: Vec<&str> = [work.categories.as_deref(), work.subjects.as_deref()]
        .into_iter()
        .flatten()
        .collect();
    let genres = genres.join(", ");
    embed_text(&EmbedFields {
        title: &work.title,
        authors: Some(&authors),
        series: Some(&series),
        genres: Some(&genres),
        description: work.description.as_deref(),
    })
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

/// Works read per page by [`embed_dirty_works`]; also the most texts one
/// embedder call receives.
pub const EMBED_PAGE_WORKS: u64 = 128;

/// What one [`embed_dirty_works`] call did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EmbedPass {
    /// Vectors written.
    pub written: usize,
    /// Whether every work was looked at. False when the deadline passed
    /// first; the next call starts over and skips what is stored.
    pub finished: bool,
}

/// Embeds `texts` in batches of the embedder's size, checking each reply.
///
/// # Errors
///
/// Returns the embedder's error, or an error when it returns another
/// number of vectors than texts or a vector of another length than its
/// dimensions.
async fn embed_checked(embedder: &mut dyn Embedder, texts: &[String]) -> Result<Vec<Vec<f32>>> {
    let mut vectors = Vec::with_capacity(texts.len());
    for batch in texts.chunks(embedder.max_batch().max(1)) {
        let made = embedder.embed(batch).await?;
        if made.len() != batch.len() {
            return Err(DiscoverError::message(format!(
                "embedder returned {} vectors for {} texts",
                made.len(),
                batch.len()
            )));
        }
        if made
            .iter()
            .any(|vector| vector.len() != embedder.dimensions())
        {
            return Err(DiscoverError::message(
                "embedder returned a vector of another length than its dimensions",
            ));
        }
        vectors.extend(made);
    }
    Ok(vectors)
}

/// The embedder's dimensions as the database stores them.
fn stored_dims(embedder: &dyn Embedder) -> Result<i64> {
    i64::try_from(embedder.dimensions())
        .map_err(|_| DiscoverError::message("embedder dimensions exceed i64"))
}

/// Embed works whose text hash changed (or are missing) in the embedder's
/// space, and drop the vectors of works that no longer exist.
///
/// Walks `works` in keyset pages of [`EMBED_PAGE_WORKS`]: each page costs
/// one read of the works, one chunked read of their stored hashes, and the
/// embedder calls for the dirty ones, so memory stays bounded by the page
/// and the store sees no per-work lookup.
///
/// # Arguments
///
/// * `library` - Open library store used for reads/writes.
/// * `embedder` - Embedding backend that produces dense vectors.
/// * `deadline` - When to stop between pages, for a caller that checks for
///   cancellation between calls; `None` to run to the end.
///
/// # Returns
///
/// How many vectors were written, and whether the walk reached the end.
///
/// # Errors
///
/// Returns an error when a store read or write fails, or when the
/// embedder fails or returns a different number of vectors than texts.
/// Pages before the failure stay stored.
pub async fn embed_dirty_works(
    library: &LibraryStore,
    embedder: &mut dyn Embedder,
    deadline: Option<Instant>,
) -> Result<EmbedPass> {
    let space = embedder.space().clone();
    let dims = stored_dims(embedder)?;
    library.prune_orphan_work_vectors().await?;
    let mut written = 0usize;
    let mut after: Option<String> = None;
    loop {
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Ok(EmbedPass {
                written,
                finished: false,
            });
        }
        let works = library
            .list_works_page(after.as_deref(), EMBED_PAGE_WORKS)
            .await?;
        let Some(last) = works.last() else {
            break;
        };
        after = Some(last.id.clone());
        let ids: Vec<String> = works.iter().map(|work| work.id.clone()).collect();
        let stored = library
            .vector_text_hashes(VECTOR_KIND_WORK, &space, &ids)
            .await?;
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
            let vectors = embed_checked(embedder, &dirty_texts).await?;
            let bytes: Vec<Vec<u8>> = vectors.iter().map(|v| vector_to_bytes(v)).collect();
            let rows: Vec<NewVector<'_>> = dirty_ids
                .iter()
                .zip(&bytes)
                .map(|((id, hash), vector)| NewVector {
                    target_id: id,
                    vector,
                    text_hash: hash,
                })
                .collect();
            library
                .put_vectors(VECTOR_KIND_WORK, &space, dims, &rows)
                .await?;
            written += rows.len();
        }
        if works.len() < usize::try_from(EMBED_PAGE_WORKS).unwrap_or(usize::MAX) {
            break;
        }
    }
    Ok(EmbedPass {
        written,
        finished: true,
    })
}

/// The space a ranking reads on a node whose embedder makes `current`.
///
/// A space **covers** the library when it holds a vector for every work.
/// The rule, in order:
///
/// 1. `current`, when it is a model's space and covers the library.
/// 2. Another model's space that covers the library (the one with the
///    newest vector): the model the library used before, or the one
///    another node embeds with while this node has none.
/// 3. The stand-in's space otherwise: its vectors are made in the ranking
///    job itself, so it always can serve.
///
/// A work added since a space last covered the library costs it the
/// cover until the work is embedded, which the next embedding does.
///
/// # Errors
///
/// Returns the library read error.
pub async fn serving_space(library: &LibraryStore, current: &VectorSpace) -> Result<VectorSpace> {
    let works = library.count_works().await?;
    let mut covering: Vec<_> = library
        .work_vector_spaces()
        .await?
        .into_iter()
        .filter(|coverage| coverage.works >= works)
        .collect();
    if works == 0 {
        return Ok(current.clone());
    }
    if let Some(found) = covering.iter().find(|c| c.space.same_as(current)) {
        if !is_stand_in(current) {
            return Ok(found.space.clone());
        }
    }
    covering.sort_by_key(|cover| std::cmp::Reverse(cover.newest));
    if let Some(model) = covering.iter().find(|c| !is_stand_in(&c.space)) {
        return Ok(model.space.clone());
    }
    Ok(stand_in_space())
}

/// Removes vectors nobody reads any more: text vectors unused for
/// [`TEXT_VECTOR_MAX_AGE`] and, once a model's space serves, the vectors
/// of every other space not written for [`OLD_SPACE_MAX_AGE`].
///
/// # Arguments
///
/// * `library` - The library store.
/// * `serving` - The space rankings read now ([`serving_space`]).
///
/// # Returns
///
/// How many vectors were removed.
///
/// # Errors
///
/// Returns the library error.
pub async fn prune_vectors(library: &LibraryStore, serving: &VectorSpace) -> Result<u64> {
    let now = chrono::Utc::now();
    let mut removed = library
        .prune_text_vectors(now - TEXT_VECTOR_MAX_AGE)
        .await?;
    if !is_stand_in(serving) {
        removed += library
            .prune_vectors_outside(serving, now - OLD_SPACE_MAX_AGE)
            .await?;
    }
    Ok(removed)
}

/// The vectors of `texts` in `space`, keyed by the SHA-256 of each text.
///
/// Stored vectors are read first. The texts without one are embedded and
/// stored when `embedder` makes vectors of `space`, until `deadline`; a
/// text that is not embedded has no entry, and the caller ranks it without
/// similarity. A vector is stored under the hash of its text, so the next
/// ranking of the same text, for anyone, reads it.
///
/// # Arguments
///
/// * `library` - The library store.
/// * `space` - The space to read and write.
/// * `embedder` - An embedder, used only when its space is `space`.
/// * `texts` - The texts; repeats are embedded once.
/// * `deadline` - When to stop embedding missing texts.
///
/// # Errors
///
/// Returns a library error. An embedder failure is not an error: the
/// texts it left stay without a vector, and it is logged.
pub async fn text_vectors(
    library: &LibraryStore,
    space: &VectorSpace,
    embedder: Option<&mut dyn Embedder>,
    texts: &[String],
    deadline: Option<Instant>,
) -> Result<HashMap<String, Vec<f32>>> {
    let mut by_hash: HashMap<String, &String> = HashMap::with_capacity(texts.len());
    for text in texts {
        by_hash.entry(text_hash(text)).or_insert(text);
    }
    let mut hashes: Vec<String> = by_hash.keys().cloned().collect();
    hashes.sort();
    let stored = library.vectors(VECTOR_KIND_TEXT, space, &hashes).await?;
    let mut vectors: HashMap<String, Vec<f32>> = stored
        .iter()
        .map(|(hash, bytes)| (hash.clone(), bytes_to_vector(bytes)))
        .collect();
    let known: Vec<String> = hashes
        .iter()
        .filter(|hash| vectors.contains_key(*hash))
        .cloned()
        .collect();
    library
        .touch_text_vectors(space, &known, chrono::Utc::now() - TEXT_VECTOR_TOUCH_AFTER)
        .await?;
    let Some(embedder) = embedder.filter(|embedder| embedder.space().same_as(space)) else {
        return Ok(vectors);
    };
    let dims = stored_dims(embedder)?;
    let missing: Vec<&String> = hashes
        .iter()
        .filter(|hash| !vectors.contains_key(*hash))
        .collect();
    for batch in missing.chunks(embedder.max_batch().max(1)) {
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            tracing::info!(
                model = %space.model,
                left = missing.len().saturating_sub(vectors.len().saturating_sub(known.len())),
                "ran out of time embedding candidate texts; the rest rank without similarity"
            );
            break;
        }
        let batch_texts: Vec<String> = batch
            .iter()
            .filter_map(|hash| by_hash.get(*hash).map(|text| (*text).clone()))
            .collect();
        let made = match embed_checked(embedder, &batch_texts).await {
            Ok(made) => made,
            Err(err) => {
                tracing::warn!(
                    model = %space.model,
                    error = %err,
                    "could not embed candidate texts; they rank without similarity"
                );
                break;
            }
        };
        let bytes: Vec<Vec<u8>> = made.iter().map(|v| vector_to_bytes(v)).collect();
        let rows: Vec<NewVector<'_>> = batch
            .iter()
            .zip(&bytes)
            .map(|(hash, vector)| NewVector {
                target_id: hash,
                vector,
                text_hash: hash,
            })
            .collect();
        library
            .put_vectors(VECTOR_KIND_TEXT, space, dims, &rows)
            .await?;
        for (hash, vector) in batch.iter().zip(made) {
            vectors.insert((*hash).clone(), vector);
        }
    }
    Ok(vectors)
}

/// An embedder several rankings share, one call at a time.
pub type SharedEmbedder = std::sync::Arc<tokio::sync::Mutex<Box<dyn Embedder>>>;

/// Where a ranking gets its vectors.
#[derive(Clone)]
pub struct VectorSource {
    /// The space the ranking reads ([`serving_space`]).
    pub space: VectorSpace,
    /// Makes the vectors of texts that have none stored; used only when
    /// its space is `space`. `None` when this node cannot make vectors of
    /// `space`: the ranking then compares only what is stored.
    pub embedder: Option<SharedEmbedder>,
}

impl VectorSource {
    /// The stand-in: hash vectors, made in the ranking itself.
    #[must_use]
    pub fn stand_in() -> Self {
        Self::new(stand_in_space(), Some(open_embedder()))
    }

    /// A source that reads `space` and embeds missing texts with `embedder`.
    #[must_use]
    pub fn new(space: VectorSpace, embedder: Option<Box<dyn Embedder>>) -> Self {
        Self {
            space,
            embedder: embedder
                .map(|embedder| std::sync::Arc::new(tokio::sync::Mutex::new(embedder))),
        }
    }
}

impl std::fmt::Debug for VectorSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VectorSource")
            .field("space", &self.space)
            .field("embedder", &self.embedder.is_some())
            .finish()
    }
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
    use bookclerk_library::NewBook;

    #[tokio::test]
    async fn hash_embed_is_stable() {
        let mut e = HashEmbedder::new(32);
        let a = e
            .embed(&[String::from("The Hobbit Tolkien")])
            .await
            .unwrap();
        let b = e
            .embed(&[String::from("The Hobbit Tolkien")])
            .await
            .unwrap();
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
        // The narrator is not part of the text.
        let expected = format!(
            "The Hobbit\nJ. R. R. Tolkien, Christopher Tolkien\nMiddle-earth\nFantasy, Dragons\n{}",
            "x".repeat(1200)
        );
        assert_eq!(text_for_work(&full), expected);
    }

    #[test]
    fn text_for_work_leaves_out_empty_fields() {
        assert_eq!(text_for_work(&work(&[], &[], &[])), "The Hobbit");
        assert_eq!(text_for_work(&work(&["A"], &[], &[])), "The Hobbit\nA");
        assert_eq!(text_for_work(&work(&[], &["N"], &[])), "The Hobbit");
        assert_eq!(text_for_work(&work(&[], &[], &["S"])), "The Hobbit\nS");
    }

    #[test]
    fn a_candidate_and_a_work_with_the_same_facts_have_the_same_text() {
        let mut held = work(&["Ursula K. Le Guin"], &["A Narrator"], &["Hainish Cycle"]);
        held.title = "The Dispossessed".into();
        held.categories = Some("Science Fiction".into());
        held.description = Some(" An ambiguous utopia. ".into());
        let candidate = embed_text(&EmbedFields {
            title: "The Dispossessed",
            authors: Some("Ursula K. Le Guin"),
            series: Some("Hainish Cycle"),
            genres: Some("Science Fiction"),
            description: Some("An ambiguous utopia."),
        });
        assert_eq!(text_for_work(&held), candidate);
        // Blank fields add no empty line.
        let bare = embed_text(&EmbedFields {
            title: " Dune ",
            authors: Some("  "),
            ..EmbedFields::default()
        });
        assert_eq!(bare, "Dune");
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
        assert_eq!(embedder.space(), &stand_in_space());
        assert!(is_stand_in(embedder.space()));
        assert_eq!(embedder.dimensions(), LOCAL_HASH_DIMENSIONS);
        let source = VectorSource::stand_in();
        assert!(source.embedder.is_some());
        assert!(format!("{source:?}").contains("local-hash-v1"));
    }

    /// An embedder standing for a model: hash vectors under a model's
    /// space, with every call recorded and failures on demand.
    struct FakeModel {
        /// Makes the vectors.
        inner: HashEmbedder,
        /// The space it claims.
        space: VectorSpace,
        /// Texts per call, in call order.
        calls: Vec<usize>,
        /// Most texts per call.
        batch: usize,
        /// When set, every call fails.
        fail: bool,
        /// When set, every call returns one vector too few.
        short: bool,
    }

    impl FakeModel {
        /// A model `model` at `revision` making 16 numbers a vector.
        fn new(model: &str, revision: &str) -> Self {
            Self {
                inner: HashEmbedder::new(16),
                space: VectorSpace {
                    model: model.into(),
                    revision: revision.into(),
                    packs: "en".into(),
                },
                calls: Vec::new(),
                batch: usize::try_from(EMBED_PAGE_WORKS).unwrap(),
                fail: false,
                short: false,
            }
        }
    }

    #[async_trait]
    impl Embedder for FakeModel {
        fn space(&self) -> &VectorSpace {
            &self.space
        }

        fn dimensions(&self) -> usize {
            self.inner.dimensions()
        }

        fn max_batch(&self) -> usize {
            self.batch
        }

        async fn embed(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
            self.calls.push(texts.len());
            if self.fail {
                return Err(DiscoverError::message("the model is away"));
            }
            let mut vectors = self.inner.embed(texts).await?;
            if self.short {
                vectors.pop();
            }
            Ok(vectors)
        }
    }

    /// A library of `total` works, one book each.
    async fn library_of(total: usize) -> LibraryStore {
        let store = bookclerk_plugin_database_sqlite::open_store_memory()
            .await
            .unwrap();
        store
            .upsert_account("audible", "acct", "us", None, true, "audible")
            .await
            .unwrap();
        for i in 0..total {
            let mut book = NewBook::minimal(format!("B{i:09}"), "acct", "us", format!("Title {i}"));
            book.authors = Some(format!("Author {i}"));
            store.upsert_book(&book).await.unwrap();
        }
        assert_eq!(store.list_works().await.unwrap().len(), total);
        store
    }

    /// How many work vectors `space` holds.
    async fn stored_works(store: &LibraryStore, space: &VectorSpace) -> u64 {
        store
            .work_vector_spaces()
            .await
            .unwrap()
            .into_iter()
            .find(|coverage| coverage.space.same_as(space))
            .map_or(0, |coverage| coverage.works)
    }

    #[tokio::test]
    async fn embeds_each_page_once_skips_clean_works_and_drops_orphans() {
        // One page and a bit, so the walk crosses a page boundary.
        let page = usize::try_from(EMBED_PAGE_WORKS).unwrap();
        let total = page + 3;
        let store = library_of(total).await;
        let mut embedder = FakeModel::new("@embedder/m", "r1");
        let space = embedder.space().clone();
        // A vector left behind by a work that no longer exists.
        store
            .put_vectors(
                VECTOR_KIND_WORK,
                &space,
                16,
                &[NewVector {
                    target_id: "deleted-work",
                    vector: &[0; 64],
                    text_hash: "stale",
                }],
            )
            .await
            .unwrap();

        let pass = embed_dirty_works(&store, &mut embedder, None)
            .await
            .unwrap();
        assert_eq!(
            pass,
            EmbedPass {
                written: total,
                finished: true
            }
        );
        assert_eq!(embedder.calls, [page, 3]);
        assert_eq!(stored_works(&store, &space).await, total as u64);
        let gone = store
            .vectors(VECTOR_KIND_WORK, &space, &[String::from("deleted-work")])
            .await
            .unwrap();
        assert!(gone.is_empty());

        // Nothing changed: no embedder call, nothing written.
        embedder.calls.clear();
        let again = embed_dirty_works(&store, &mut embedder, None)
            .await
            .unwrap();
        assert_eq!(again.written, 0);
        assert!(again.finished);
        assert!(embedder.calls.is_empty());
    }

    #[tokio::test]
    async fn a_pass_stops_at_its_deadline_and_the_next_one_goes_on() {
        let page = usize::try_from(EMBED_PAGE_WORKS).unwrap();
        let store = library_of(page + 3).await;
        let mut embedder = FakeModel::new("@embedder/m", "r1");
        embedder.batch = 50;
        // Already past: nothing is read or embedded.
        let stopped = embed_dirty_works(&store, &mut embedder, Some(Instant::now()))
            .await
            .unwrap();
        assert_eq!(stopped, EmbedPass::default());
        assert!(embedder.calls.is_empty());
        // A page is embedded in batches of the embedder's size.
        let done = embed_dirty_works(&store, &mut embedder, None)
            .await
            .unwrap();
        assert!(done.finished);
        assert_eq!(embedder.calls, [50, 50, 28, 3]);
    }

    #[tokio::test]
    async fn a_failing_or_short_embedder_is_an_error_for_works() {
        let store = library_of(2).await;
        let mut away = FakeModel::new("@embedder/m", "r1");
        away.fail = true;
        assert!(embed_dirty_works(&store, &mut away, None).await.is_err());
        let mut short = FakeModel::new("@embedder/m", "r1");
        short.short = true;
        let err = embed_dirty_works(&store, &mut short, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("1 vectors for 2 texts"), "{err}");
        assert_eq!(stored_works(&store, short.space()).await, 0);
    }

    #[tokio::test]
    async fn the_old_space_serves_until_the_new_model_covers_every_work() {
        let store = library_of(3).await;
        let stand_in = stand_in_space();
        let mut hash = HashEmbedder::new(16);
        let mut first = FakeModel::new("@embedder/m", "r1");
        let r1 = first.space().clone();

        // Nothing stored: only the stand-in can serve, whoever asks.
        assert_eq!(serving_space(&store, &stand_in).await.unwrap(), stand_in);
        assert_eq!(serving_space(&store, &r1).await.unwrap(), stand_in);

        // The stand-in covers the library; a model that has not embedded
        // yet does not take over.
        embed_dirty_works(&store, &mut hash, None).await.unwrap();
        assert_eq!(serving_space(&store, &r1).await.unwrap(), stand_in);

        // Part of the library is not enough.
        let ids: Vec<String> = store
            .list_works()
            .await
            .unwrap()
            .into_iter()
            .map(|work| work.id)
            .collect();
        store
            .put_vectors(
                VECTOR_KIND_WORK,
                &r1,
                16,
                &[NewVector {
                    target_id: &ids[0],
                    vector: &[0; 64],
                    text_hash: "partial",
                }],
            )
            .await
            .unwrap();
        assert_eq!(serving_space(&store, &r1).await.unwrap(), stand_in);

        // Once the model has every work, it serves: on this node, and on a
        // node that has no model (whose own embedder is the stand-in).
        embed_dirty_works(&store, &mut first, None).await.unwrap();
        assert_eq!(serving_space(&store, &r1).await.unwrap(), r1);
        assert_eq!(serving_space(&store, &stand_in).await.unwrap(), r1);

        // The model changed (another revision, or another model): the old
        // vectors keep serving until the new ones cover the library.
        let mut second = FakeModel::new("@embedder/m", "r2");
        let r2 = second.space().clone();
        assert_eq!(serving_space(&store, &r2).await.unwrap(), r1);
        embed_dirty_works(&store, &mut second, None).await.unwrap();
        assert_eq!(serving_space(&store, &r2).await.unwrap(), r2);

        // The spaces the library left stay for a day, so a node still on
        // one keeps its vectors, and go after it.
        assert_eq!(prune_vectors(&store, &r2).await.unwrap(), 0);
        let later = chrono::Utc::now() + chrono::Duration::hours(1);
        assert_eq!(store.prune_vectors_outside(&r2, later).await.unwrap(), 6);
        assert_eq!(serving_space(&store, &stand_in).await.unwrap(), r2);
        // The stand-in never removes a model's vectors.
        assert_eq!(prune_vectors(&store, &stand_in).await.unwrap(), 0);
        assert_eq!(stored_works(&store, &r2).await, 3);

        // A new work costs the model its cover until it is embedded.
        let book = NewBook::minimal("B-NEW", "acct", "us", "A New One");
        store.upsert_book(&book).await.unwrap();
        assert_eq!(serving_space(&store, &r2).await.unwrap(), stand_in);
        embed_dirty_works(&store, &mut second, None).await.unwrap();
        assert_eq!(serving_space(&store, &r2).await.unwrap(), r2);
    }

    #[tokio::test]
    async fn an_empty_library_serves_the_nodes_own_space() {
        let store = library_of(0).await;
        let model = FakeModel::new("@embedder/m", "r1");
        assert_eq!(
            serving_space(&store, model.space()).await.unwrap(),
            *model.space()
        );
    }

    #[tokio::test]
    async fn a_text_is_embedded_once_and_shared() {
        let store = library_of(0).await;
        let mut model = FakeModel::new("@embedder/m", "r1");
        model.batch = 2;
        let space = model.space().clone();
        let texts = vec![
            String::from("Dune\nFrank Herbert"),
            String::from("Hyperion\nDan Simmons"),
            String::from("Dune\nFrank Herbert"),
            String::from("Ubik\nPhilip K. Dick"),
        ];
        let first = text_vectors(&store, &space, Some(&mut model), &texts, None)
            .await
            .unwrap();
        // Three distinct texts, in batches of two.
        assert_eq!(first.len(), 3);
        assert_eq!(model.calls, [2, 1]);
        assert!(first.contains_key(&text_hash("Dune\nFrank Herbert")));

        // The second ranking reads them: no call, with or without an
        // embedder.
        model.calls.clear();
        let second = text_vectors(&store, &space, Some(&mut model), &texts, None)
            .await
            .unwrap();
        assert_eq!(second, first);
        let stored_only = text_vectors(&store, &space, None, &texts, None)
            .await
            .unwrap();
        assert_eq!(stored_only, first);
        assert!(model.calls.is_empty());

        // Another space shares nothing, and an embedder of another space
        // is not used to fill it.
        let other = VectorSpace {
            revision: "r2".into(),
            ..space.clone()
        };
        let none = text_vectors(&store, &other, Some(&mut model), &texts, None)
            .await
            .unwrap();
        assert!(none.is_empty());
        assert!(model.calls.is_empty());
    }

    #[tokio::test]
    async fn texts_left_without_a_vector_are_not_an_error() {
        let store = library_of(0).await;
        let texts = vec![String::from("Dune"), String::from("Hyperion")];

        // Out of time before the first batch.
        let mut model = FakeModel::new("@embedder/m", "r1");
        let space = model.space().clone();
        let late = text_vectors(
            &store,
            &space,
            Some(&mut model),
            &texts,
            Some(Instant::now()),
        )
        .await
        .unwrap();
        assert!(late.is_empty());
        assert!(model.calls.is_empty());

        // The model is away.
        model.fail = true;
        let away = text_vectors(&store, &space, Some(&mut model), &texts, None)
            .await
            .unwrap();
        assert!(away.is_empty());
        assert_eq!(model.calls, [2]);

        // It answers with too few vectors: nothing of the batch is stored.
        model.fail = false;
        model.short = true;
        let short = text_vectors(&store, &space, Some(&mut model), &texts, None)
            .await
            .unwrap();
        assert!(short.is_empty());
        let stored = text_vectors(&store, &space, None, &texts, None)
            .await
            .unwrap();
        assert!(stored.is_empty());
    }
}
