//! The axes of a taste radar: the genres a person's taste leans to, each
//! with the point in the embedding space where their books of that genre
//! sit.
//!
//! A radar shows how close one book is to each part of a person's taste.
//! Its axes are named by the genre labels the person's seed works carry,
//! so an axis is a word a person knows ("Fantasy"), and the same taste
//! always gives the same axes. Where an axis points is not the genre as
//! the world files it but the person's own books of that genre: the mean
//! of their vectors. A book is then placed on an axis by how close its
//! vector is to that mean ([`TasteAxis::centroid`]), which the caller
//! measures against its own library.
//!
//! See `docs/internals/discovery.md`, *The taste radar*.

use bookclerk_library::{DiscoverSubject, LibraryStore};
use chrono::{DateTime, Utc};

use crate::error::Result;
use crate::precompute::genre_labels;
use crate::stored_taste::StoredTaste;

/// Most axes a radar has. More wedges than this are too thin to read or
/// to touch on a phone.
pub const RADAR_MAX_AXES: usize = 6;

/// Fewest axes that make a radar: two points are a line, not a shape.
pub const RADAR_MIN_AXES: usize = 3;

/// Fewest seed works a genre label needs to be an axis. One book is a
/// book, not a leaning.
pub const RADAR_AXIS_MIN_WORKS: usize = 2;

/// A label carried by more than this share of the seeds (numerator,
/// denominator) is no axis: it says what the whole taste is, not a part
/// of it.
const WHOLE_TASTE: (usize, usize) = (4, 5);

/// Two labels whose seed works overlap by more than this share of their
/// union (numerator, denominator) are one axis; the more common label
/// names it.
const SAME_AXIS: (usize, usize) = (4, 5);

/// One seed work of a taste, as an axis is built from it.
#[derive(Debug, Clone)]
pub struct RadarSeed {
    /// The work.
    pub work_id: String,
    /// The work's genres, categories or subjects, as stored (one line of
    /// comma-, semicolon- or `>`-separated labels).
    pub genres: Option<String>,
    /// The work's vector in the space the radar is read in.
    pub vector: Vec<f32>,
}

/// One axis of a taste radar.
#[derive(Debug, Clone, PartialEq)]
pub struct TasteAxis {
    /// The genre label that names the axis, as the first seed wrote it.
    pub name: String,
    /// The mean of the vectors of the seed works that carry the label. Not
    /// unit length.
    pub centroid: Vec<f32>,
    /// The seed works that carry the label, in the order given.
    pub work_ids: Vec<String>,
}

/// The seed works of one subject's stored taste.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredSeeds {
    /// When the taste was stored. A newer time means other seeds.
    pub computed_at: DateTime<Utc>,
    /// The seed works, in id order.
    pub work_ids: Vec<String>,
}

/// The seed works of `subject`'s stored taste: the works whose vectors
/// make the centroid their shelves are ranked by.
///
/// Reads only what a candidate fetch stored. A taste past its age is
/// still read: a radar a day old is better than none, and the next fetch
/// replaces it.
///
/// # Returns
///
/// `None` when the subject has no stored taste this release can read.
///
/// # Errors
///
/// Returns the library read error.
pub async fn stored_taste_seeds(
    library: &LibraryStore,
    subject: &DiscoverSubject,
) -> Result<Option<StoredSeeds>> {
    let Some(row) = library.get_discover_taste(subject).await? else {
        return Ok(None);
    };
    Ok(
        StoredTaste::decode(&row.taste_json).map(|taste| StoredSeeds {
            computed_at: row.computed_at,
            work_ids: taste.scores.seed_work_ids.into_iter().collect(),
        }),
    )
}

/// Whether `part` is more than the share `of` (numerator, denominator) of
/// `whole`.
fn more_than(part: usize, whole: usize, of: (usize, usize)) -> bool {
    part.saturating_mul(of.1) > whole.saturating_mul(of.0)
}

/// The axes of the taste `seeds` make: the genre labels most seeds carry,
/// most common first, at most [`RADAR_MAX_AXES`].
///
/// A label is an axis when at least [`RADAR_AXIS_MIN_WORKS`] seeds carry
/// it, it is not carried by nearly every seed, and its seeds are not
/// nearly the same as those of an axis already taken ("Epic Fantasy"
/// under "Fantasy" when every fantasy book is epic). Labels that differ
/// only in case are one, and equal counts come in the labels' own order.
/// Seeds whose vector differs in length from the first one's are left out.
///
/// # Arguments
///
/// * `seeds` - The seed works with a vector, in any order.
///
/// # Returns
///
/// The axes, or none when fewer than [`RADAR_MIN_AXES`] labels qualify.
#[must_use]
pub fn taste_axes(seeds: &[RadarSeed]) -> Vec<TasteAxis> {
    let Some(dims) = seeds.first().map(|seed| seed.vector.len()) else {
        return Vec::new();
    };
    let seeds: Vec<&RadarSeed> = seeds
        .iter()
        .filter(|seed| dims > 0 && seed.vector.len() == dims)
        .collect();
    // (label as first written, lowercase key, positions of its seeds)
    let mut labels: Vec<(String, String, Vec<usize>)> = Vec::new();
    let mut labelled = 0_usize;
    for (position, seed) in seeds.iter().enumerate() {
        let found = seed.genres.as_deref().map(genre_labels).unwrap_or_default();
        if !found.is_empty() {
            labelled += 1;
        }
        for (label, key) in found {
            match labels.iter_mut().find(|(_, of, _)| *of == key) {
                Some((_, _, positions)) => positions.push(position),
                None => labels.push((label, key, vec![position])),
            }
        }
    }
    // Equal counts in the order of the labels, not of the seeds: seeds come
    // in an order that means nothing to a person.
    labels.sort_by(|a, b| b.2.len().cmp(&a.2.len()).then_with(|| a.1.cmp(&b.1)));
    let mut taken: Vec<(String, Vec<usize>)> = Vec::new();
    for (label, _, positions) in labels {
        if taken.len() == RADAR_MAX_AXES || positions.len() < RADAR_AXIS_MIN_WORKS {
            break;
        }
        if more_than(positions.len(), labelled, WHOLE_TASTE) {
            continue;
        }
        let repeats = taken.iter().any(|(_, earlier)| {
            let shared = positions.iter().filter(|p| earlier.contains(p)).count();
            let union = positions.len() + earlier.len() - shared;
            more_than(shared, union, SAME_AXIS)
        });
        if !repeats {
            taken.push((label, positions));
        }
    }
    if taken.len() < RADAR_MIN_AXES {
        return Vec::new();
    }
    taken
        .into_iter()
        .map(|(name, positions)| {
            let mut centroid = vec![0.0_f32; dims];
            for position in &positions {
                for (total, x) in centroid.iter_mut().zip(&seeds[*position].vector) {
                    *total += x;
                }
            }
            // A count of seed works, far below f32's exact integers.
            #[allow(clippy::cast_precision_loss)]
            let count = positions.len() as f32;
            for total in &mut centroid {
                *total /= count;
            }
            TasteAxis {
                name,
                centroid,
                work_ids: positions
                    .into_iter()
                    .map(|position| seeds[position].work_id.clone())
                    .collect(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A seed with `genres` pointing along `vector`.
    fn seed(work_id: &str, genres: &str, vector: [f32; 2]) -> RadarSeed {
        RadarSeed {
            work_id: work_id.to_string(),
            genres: (!genres.is_empty()).then(|| genres.to_string()),
            vector: vector.to_vec(),
        }
    }

    /// The axis names of `seeds`.
    fn names(seeds: &[RadarSeed]) -> Vec<String> {
        taste_axes(seeds)
            .into_iter()
            .map(|axis| axis.name)
            .collect()
    }

    #[test]
    fn axes_are_the_labels_most_seeds_carry_with_their_mean() {
        let seeds = [
            seed("a", "Fantasy, Humor", [1.0, 0.0]),
            seed("b", "fantasy; Mystery", [0.0, 1.0]),
            seed("c", "Mystery > Humor", [1.0, 1.0]),
            seed("d", "Fantasy, History", [1.0, 0.0]),
            seed("e", "", [9.0, 9.0]),
        ];
        let axes = taste_axes(&seeds);
        assert_eq!(
            axes.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(),
            ["Fantasy", "Humor", "Mystery"],
            "most common first, then by name; one book's History is no axis"
        );
        assert_eq!(axes[0].work_ids, ["a", "b", "d"]);
        assert_eq!(axes[0].centroid, [2.0 / 3.0, 1.0 / 3.0]);
        assert_eq!(axes[2].work_ids, ["b", "c"]);
        assert_eq!(axes[2].centroid, [0.5, 1.0]);
    }

    #[test]
    fn fewer_than_three_axes_make_no_radar() {
        let seeds = [
            seed("a", "Fantasy, Humor", [1.0, 0.0]),
            seed("b", "Fantasy, Humor, Mystery", [0.0, 1.0]),
            seed("c", "Mystery", [0.0, 1.0]),
        ];
        // Fantasy and Humor name the same two books: one axis, with Mystery two.
        assert!(taste_axes(&seeds).is_empty());
        assert!(taste_axes(&[]).is_empty());
    }

    #[test]
    fn a_label_on_nearly_every_seed_and_broad_labels_are_no_axes() {
        let mut seeds: Vec<RadarSeed> = (0..10)
            .map(|i| {
                let part = ["Epic", "Cozy", "Dark", "Urban", "Space"][i % 5];
                seed(
                    &format!("w{i}"),
                    &format!("Fiction, Science Fiction & Fantasy > {part}"),
                    [1.0, 0.0],
                )
            })
            .collect();
        assert_eq!(names(&seeds), ["Cozy", "Dark", "Epic", "Space", "Urban"]);
        // At most six axes however many labels there are.
        seeds.extend((0..6).map(|i| seed(&format!("x{i}"), "A, B, C, D, E, F, G", [0.0, 1.0])));
        assert_eq!(taste_axes(&seeds).len(), RADAR_MAX_AXES);
    }

    #[test]
    fn seeds_of_another_length_are_left_out() {
        let mut seeds = vec![
            seed("a", "Fantasy, Humor, Mystery", [1.0, 0.0]),
            seed("b", "Fantasy, Humor", [0.0, 1.0]),
            seed("c", "Mystery, Poetry", [0.0, 1.0]),
            seed("d", "Poetry", [1.0, 1.0]),
        ];
        seeds.push(RadarSeed {
            work_id: "long".to_string(),
            genres: Some("Fantasy".to_string()),
            vector: vec![1.0, 2.0, 3.0],
        });
        let axes = taste_axes(&seeds);
        assert!(axes
            .iter()
            .all(|axis| !axis.work_ids.contains(&"long".to_string())));
        assert_eq!(axes.len(), 3, "Fantasy and Humor are one axis");
    }
}
