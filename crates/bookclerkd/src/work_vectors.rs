//! This node's in-memory index of the library's work vectors.
//!
//! The durable copy of every vector is the `embeddings` table. A node
//! that answers "which works are nearest to this vector" keeps the work
//! vectors of the space that serves the library
//! ([`bookclerk_discover::serving_space`]) in one flat matrix and scans
//! all of it: an exact scan of 10,000 vectors takes a few milliseconds, so
//! there is no approximate index and no engine-specific vector type.
//!
//! The matrix is rebuilt from the table when the space's row count or its
//! newest write changed ([`WorkVectors::current`] looks at most every
//! [`REVALIDATE_EVERY`]; the `discover_embed` job of this node asks for a
//! look at once when it ends). Search by meaning and "more like this" in
//! [`crate::library_works`] read it. See `docs/internals/discovery.md`,
//! *Searching by meaning*.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bookclerk_library::{LibraryStore, VectorSpace};
use chrono::{DateTime, Utc};
use tokio::sync::Mutex;

/// Least time between two looks at the table for a change, so a burst of
/// searches costs one small read.
const REVALIDATE_EVERY: Duration = Duration::from_secs(5);

/// Query vectors kept, so the pages of one search embed its text once.
const QUERY_CACHE_ENTRIES: usize = 32;

/// How many standard deviations above the library's mean similarity a
/// work must stand to be a match of a search by meaning. A model's
/// cosines sit in a narrow band, so a fixed cosine would admit everything
/// or nothing; standing within this library is what tells a match from
/// the rest. A first setting, not yet measured on real libraries.
pub(crate) const MATCH_STANDING: f32 = 1.0;

/// The work vectors of one space, unit length, row by row.
#[derive(Debug)]
pub(crate) struct WorkMatrix {
    /// The space the vectors belong to.
    space: VectorSpace,
    /// Numbers in each vector; 0 when the space holds none.
    dims: usize,
    /// Work id of each row.
    ids: Vec<String>,
    /// Row of each work id.
    rows: HashMap<String, usize>,
    /// `ids.len() * dims` numbers; each row has length 1 (or is all zero).
    data: Vec<f32>,
}

/// One work and how alike it is to what was asked.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Scored {
    /// The work.
    pub(crate) work_id: String,
    /// Cosine similarity, -1 to 1.
    pub(crate) score: f32,
}

/// `vector` scaled to length 1; unchanged when it is all zero.
///
/// The numbers are first divided by the largest of them, so the sum of
/// squares neither overflows for a huge vector nor vanishes for a tiny one.
fn unit(vector: &mut [f32]) {
    let largest = vector.iter().fold(0.0_f32, |max, x| max.max(x.abs()));
    if largest <= 0.0 || !largest.is_finite() {
        return;
    }
    for x in vector.iter_mut() {
        *x /= largest;
    }
    let norm = vector.iter().map(|x| x * x).sum::<f32>().sqrt();
    for x in vector {
        *x /= norm;
    }
}

impl WorkMatrix {
    /// Builds the matrix from stored vectors (little-endian `f32` bytes).
    /// A vector whose length differs from the first one's is left out.
    fn build(space: VectorSpace, stored: Vec<(String, Vec<u8>)>) -> Self {
        let mut matrix = Self {
            space,
            dims: 0,
            ids: Vec::with_capacity(stored.len()),
            rows: HashMap::with_capacity(stored.len()),
            data: Vec::new(),
        };
        for (work_id, bytes) in stored {
            let mut vector = bookclerk_discover::bytes_to_vector(&bytes);
            if vector.is_empty() {
                continue;
            }
            if matrix.dims == 0 {
                matrix.dims = vector.len();
                matrix.data.reserve(matrix.dims * matrix.ids.capacity());
            }
            if vector.len() != matrix.dims {
                continue;
            }
            unit(&mut vector);
            matrix.rows.insert(work_id.clone(), matrix.ids.len());
            matrix.ids.push(work_id);
            matrix.data.extend(vector);
        }
        matrix
    }

    /// How many works have a vector.
    pub(crate) fn len(&self) -> usize {
        self.ids.len()
    }

    /// The vector of `work_id`, when it has one.
    pub(crate) fn vector_of(&self, work_id: &str) -> Option<&[f32]> {
        let row = *self.rows.get(work_id)?;
        self.data.get(row * self.dims..(row + 1) * self.dims)
    }

    /// Every work's similarity to `query`, best first; ties in work id
    /// order, so two pages of one search agree.
    ///
    /// # Arguments
    ///
    /// * `query` - A vector of this space; any length other than the
    ///   matrix's gives no rows.
    /// * `except` - A work to leave out (the one "more like this" is for).
    pub(crate) fn ranked(&self, query: &[f32], except: Option<&str>) -> Vec<Scored> {
        if self.dims == 0 || query.len() != self.dims {
            return Vec::new();
        }
        let mut query = query.to_vec();
        unit(&mut query);
        let mut scored: Vec<Scored> = self
            .ids
            .iter()
            .zip(self.data.chunks_exact(self.dims))
            .filter(|(work_id, _)| except != Some(work_id.as_str()))
            .map(|(work_id, row)| Scored {
                work_id: work_id.clone(),
                score: row.iter().zip(&query).map(|(a, b)| a * b).sum(),
            })
            .collect();
        scored.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| a.work_id.cmp(&b.work_id))
        });
        scored
    }
}

/// The leading rows of `ranked` (best first) that stand at least
/// `standing` standard deviations above the mean of all its scores.
///
/// With fewer than three rows there is no spread to stand out from, and
/// every row is kept.
pub(crate) fn standing_out(ranked: Vec<Scored>, standing: f32) -> Vec<Scored> {
    if ranked.len() < 3 {
        return ranked;
    }
    let n = ranked.iter().fold(0.0_f32, |n, _| n + 1.0);
    let mean = ranked.iter().map(|s| s.score).sum::<f32>() / n;
    let deviation = (ranked.iter().map(|s| (s.score - mean).powi(2)).sum::<f32>() / n).sqrt();
    if deviation <= f32::EPSILON {
        return ranked;
    }
    let floor = mean + standing * deviation;
    ranked
        .into_iter()
        .take_while(|s| s.score >= floor)
        .collect()
}

/// What the matrix was built from: a change of either means a rebuild.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Stamp {
    /// Work vectors the space held.
    works: u64,
    /// When the newest of them was written.
    newest: Option<DateTime<Utc>>,
}

/// The matrix in memory and when the table was last looked at.
#[derive(Debug, Default)]
struct Held {
    /// The matrix and the stamp it was built at.
    matrix: Option<(Arc<WorkMatrix>, Stamp)>,
    /// When the table was last looked at for a change.
    looked: Option<Instant>,
    /// Query vectors by `(space, text)`, oldest first.
    queries: VecDeque<(VectorSpace, String, Arc<Vec<f32>>)>,
}

/// This node's index of work vectors. One per daemon.
#[derive(Debug, Default)]
pub(crate) struct WorkVectors {
    /// The matrix; the lock is held across a rebuild, so two requests
    /// that find it stale read the table once.
    held: Mutex<Held>,
}

impl WorkVectors {
    /// The matrix of `space`, rebuilt first when the table changed since
    /// it was built or it holds another space.
    ///
    /// # Errors
    ///
    /// Returns the library read error.
    pub(crate) async fn current(
        &self,
        library: &LibraryStore,
        space: &VectorSpace,
    ) -> bookclerk_library::Result<Arc<WorkMatrix>> {
        let mut held = self.held.lock().await;
        if let (Some((matrix, _)), Some(looked)) = (&held.matrix, held.looked) {
            if matrix.space.same_as(space) && looked.elapsed() < REVALIDATE_EVERY {
                return Ok(Arc::clone(matrix));
            }
        }
        let stamp = library
            .work_vector_spaces()
            .await?
            .into_iter()
            .find(|coverage| coverage.space.same_as(space))
            .map_or(
                Stamp {
                    works: 0,
                    newest: None,
                },
                |coverage| Stamp {
                    works: coverage.works,
                    newest: coverage.newest,
                },
            );
        held.looked = Some(Instant::now());
        if let Some((matrix, built_at)) = &held.matrix {
            if matrix.space.same_as(space) && *built_at == stamp {
                return Ok(Arc::clone(matrix));
            }
        }
        let stored = library.work_vectors_in(space).await?;
        let matrix = Arc::new(WorkMatrix::build(space.clone(), stored));
        tracing::debug!(
            model = %space.model,
            works = matrix.len(),
            "rebuilt the work vector index"
        );
        held.matrix = Some((Arc::clone(&matrix), stamp));
        Ok(matrix)
    }

    /// Makes the next [`Self::current`] look at the table at once: this
    /// node just wrote vectors.
    pub(crate) async fn changed(&self) {
        self.held.lock().await.looked = None;
    }

    /// Forgets the matrix and every query vector: the daemon reads another
    /// library store now, whose vectors may carry the same stamp.
    pub(crate) async fn reset(&self) {
        *self.held.lock().await = Held::default();
    }

    /// The vector an earlier page of the same search made for `text`.
    pub(crate) async fn cached_query(
        &self,
        space: &VectorSpace,
        text: &str,
    ) -> Option<Arc<Vec<f32>>> {
        self.held
            .lock()
            .await
            .queries
            .iter()
            .find(|(of, query, _)| of.same_as(space) && query == text)
            .map(|(_, _, vector)| Arc::clone(vector))
    }

    /// Keeps the vector of `text` for the search's later pages.
    pub(crate) async fn cache_query(&self, space: &VectorSpace, text: &str, vector: Arc<Vec<f32>>) {
        let mut held = self.held.lock().await;
        held.queries
            .retain(|(of, query, _)| !(of.same_as(space) && query == text));
        while held.queries.len() >= QUERY_CACHE_ENTRIES {
            held.queries.pop_front();
        }
        held.queries
            .push_back((space.clone(), text.to_string(), vector));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A space named `model`.
    fn space(model: &str) -> VectorSpace {
        VectorSpace {
            model: model.into(),
            revision: "r1".into(),
            packs: String::new(),
        }
    }

    /// `(work id, vector)` as the store returns it.
    fn stored(work_id: &str, vector: &[f32]) -> (String, Vec<u8>) {
        (
            work_id.to_string(),
            bookclerk_discover::vector_to_bytes(vector),
        )
    }

    #[test]
    fn ranks_by_cosine_whatever_the_stored_length() {
        let matrix = WorkMatrix::build(
            space("m"),
            vec![
                stored("east", &[10.0, 0.0]),
                stored("north", &[0.0, 0.5]),
                stored("north-east", &[3.0, 3.0]),
                stored("short", &[1.0]),
                stored("empty", &[]),
            ],
        );
        assert_eq!(matrix.len(), 3);
        let ranked = matrix.ranked(&[2.0, 0.0], None);
        let ids: Vec<&str> = ranked.iter().map(|s| s.work_id.as_str()).collect();
        assert_eq!(ids, ["east", "north-east", "north"]);
        assert!((ranked[0].score - 1.0).abs() < 1e-6);
        assert!(ranked[2].score.abs() < 1e-6);

        // The work itself is left out of what is like it.
        let like_east = matrix.ranked(matrix.vector_of("east").unwrap(), Some("east"));
        assert_eq!(like_east.len(), 2);
        assert_eq!(like_east[0].work_id, "north-east");
        assert!(matrix.vector_of("short").is_none());

        // A vector of another length belongs to another space.
        assert!(matrix.ranked(&[1.0, 0.0, 0.0], None).is_empty());
        assert!(WorkMatrix::build(space("m"), Vec::new())
            .ranked(&[1.0], None)
            .is_empty());
    }

    #[test]
    fn huge_and_tiny_vectors_keep_their_direction() {
        let matrix = WorkMatrix::build(
            space("m"),
            vec![stored("east", &[1e20, 0.0]), stored("north", &[0.0, 1e-20])],
        );
        for query in [[1e20_f32, 0.0], [1e-20, 0.0]] {
            let ranked = matrix.ranked(&query, None);
            assert_eq!(ranked[0].work_id, "east");
            assert!((ranked[0].score - 1.0).abs() < 1e-6, "{ranked:?}");
            assert!(ranked[1].score.abs() < 1e-6, "{ranked:?}");
        }
        // Nothing to scale: all zero, or not a number.
        let mut zero = [0.0_f32, 0.0];
        unit(&mut zero);
        assert_eq!(zero, [0.0, 0.0]);
        let mut endless = [f32::INFINITY, 1.0];
        unit(&mut endless);
        assert_eq!(endless, [f32::INFINITY, 1.0]);
    }

    #[tokio::test]
    async fn a_reset_forgets_the_query_vectors() {
        let index = WorkVectors::default();
        let m = space("m");
        index.cache_query(&m, "q", Arc::new(vec![1.0])).await;
        index.reset().await;
        assert!(index.cached_query(&m, "q").await.is_none());
    }

    #[test]
    fn ties_come_in_work_id_order() {
        let matrix = WorkMatrix::build(
            space("m"),
            vec![stored("b", &[1.0, 0.0]), stored("a", &[1.0, 0.0])],
        );
        let ranked = matrix.ranked(&[1.0, 0.0], None);
        assert_eq!(ranked[0].work_id, "a");
        assert_eq!(ranked[1].work_id, "b");
    }

    #[test]
    fn keeps_only_what_stands_out() {
        let scored = |scores: &[f32]| -> Vec<Scored> {
            scores
                .iter()
                .enumerate()
                .map(|(i, score)| Scored {
                    work_id: format!("w{i}"),
                    score: *score,
                })
                .collect()
        };
        // One work far above a flat rest.
        let kept = standing_out(scored(&[0.9, 0.5, 0.5, 0.5, 0.5, 0.5]), 1.0);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].work_id, "w0");
        // No spread, or too few rows to have one: everything is kept.
        assert_eq!(standing_out(scored(&[0.5, 0.5, 0.5]), 1.0).len(), 3);
        assert_eq!(standing_out(scored(&[0.9, 0.1]), 1.0).len(), 2);
        assert!(standing_out(Vec::new(), 1.0).is_empty());
    }

    #[tokio::test]
    async fn the_query_cache_forgets_the_oldest() {
        let index = WorkVectors::default();
        let m = space("m");
        for i in 0..=QUERY_CACHE_ENTRIES {
            index
                .cache_query(&m, &format!("q{i}"), Arc::new(vec![1.0]))
                .await;
        }
        assert!(index.cached_query(&m, "q0").await.is_none());
        assert!(index.cached_query(&m, "q1").await.is_some());
        // Another space's vector is not this one's.
        assert!(index.cached_query(&space("other"), "q1").await.is_none());
        // Storing a text again replaces its vector.
        index.cache_query(&m, "q1", Arc::new(vec![2.0])).await;
        assert_eq!(*index.cached_query(&m, "q1").await.unwrap(), vec![2.0]);
    }
}
