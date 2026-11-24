//! The axes of a taste: the few directions a person's books and wishes
//! point in, found by grouping them by meaning.
//!
//! One average of everything a person likes sits between their tastes and
//! is close to none of them: the mean of hard science fiction and cosy
//! mysteries is neither. So the seed works of a taste are split into
//! three to six groups whose vectors sit together ([`cluster`]), whatever
//! genre a store files them under. Each group is an axis: it points at the
//! weighted mean of its works ([`axis_centroid`]) and is named by what its
//! works have in common and the person's other works lack ([`name_axes`]):
//! a genre, a series, or a phrase their descriptions share.
//!
//! The axes are stored with the subject's taste
//! ([`crate::stored_taste::StoredTaste`]) as their names and member works,
//! not as vectors, so the cell stays small and the axes survive a change
//! of model. A ranking scores candidates against every axis, and the taste
//! radar of a book draws the same ones, so the chart explains the
//! shelves. See `docs/internals/discovery.md`, *Taste axes*.

use std::collections::{BTreeMap, HashMap};

use bookclerk_library::{DiscoverSubject, LibraryStore, VectorSpace, WorkRecord};
use chrono::{DateTime, Utc};

use crate::embed::bytes_to_vector;
use crate::error::Result;
use crate::precompute::genre_labels;
use crate::stored_taste::StoredTaste;

/// Most axes a taste has. More wedges than this are too thin to read on a
/// radar or to touch on a phone.
pub const MAX_TASTE_AXES: usize = 6;

/// Fewest axes that are worth having: with two, one average already says
/// most of it, and two points make no shape on a radar.
pub const MIN_TASTE_AXES: usize = 3;

/// Fewest works on an axis. One book is a book, not a leaning.
pub const AXIS_MIN_WORKS: usize = 2;

/// Most seed works grouped, heaviest first. Bounds the grouping and the
/// stored cell for a library of many thousands of works.
pub const MAX_AXIS_SEEDS: usize = 400;

/// What an open wish weighs as a seed. More than a book that is only in
/// the library (0.5, less still when it is the household's), less than a
/// book the person finished (3) or rated four stars or more (2): a wish
/// says "I want this", a finish says "I read all of it".
pub const WISH_SEED_WEIGHT: f64 = 1.5;

/// Rounds of reassignment after which the grouping stops even if works
/// still move.
const CLUSTER_ROUNDS: usize = 25;

/// Share of an axis's works (numerator, denominator) that must carry a
/// genre or series for it to name the axis.
const LABEL_MIN_SHARE: (usize, usize) = (1, 3);

/// Share of an axis's works (numerator, denominator) whose descriptions
/// must hold a phrase for it to name the axis.
const PHRASE_MIN_SHARE: (usize, usize) = (1, 2);

/// How much more distinctive a description phrase must be than a genre or
/// series to name the axis instead. Phrases are noisier than labels.
const PHRASE_HANDICAP: f64 = 0.15;

/// The least a name's distinctiveness may be: its share among the axis's
/// works less its share among the person's other works.
const NAME_MIN_DISTINCT: f64 = 0.2;

/// Characters of a description read for phrases: what is embedded of it.
const PHRASE_DESCRIPTION_CHARS: usize = 1200;

/// Words a description phrase may not start or end with.
const STOPWORDS: [&str; 62] = [
    "a", "an", "the", "of", "and", "or", "in", "on", "to", "for", "with", "by", "from", "at", "is",
    "are", "was", "were", "be", "been", "this", "that", "these", "those", "it", "its", "his",
    "her", "their", "he", "she", "they", "as", "but", "not", "has", "have", "had", "will", "who",
    "whom", "which", "what", "when", "where", "into", "than", "then", "so", "if", "you", "your",
    "our", "we", "i", "him", "them", "about", "after", "before", "all", "s",
];

/// One work on a taste axis.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AxisMember {
    /// The work.
    pub work_id: String,
    /// How much it counts toward where the axis points.
    pub weight: f64,
}

/// One axis of a taste, as it is stored.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TasteAxis {
    /// What the axis's works have in common, for display: a genre, a
    /// series, a phrase of their descriptions, or "Like " and a title.
    pub name: String,
    /// The works on the axis, heaviest first.
    pub members: Vec<AxisMember>,
}

/// The axes of one subject's stored taste.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredAxes {
    /// When the taste was stored. A newer time means other axes.
    pub computed_at: DateTime<Utc>,
    /// The axes, the one with the most weight first; empty when the taste
    /// has too few seed works that sit together.
    pub axes: Vec<TasteAxis>,
}

/// The axes of `subject`'s stored taste.
///
/// Reads only what a candidate fetch or a ranking stored. A taste past its
/// age is still read: axes a day old are better than none, and the next
/// fetch replaces them.
///
/// # Returns
///
/// `None` when the subject has no stored taste this release can read.
///
/// # Errors
///
/// Returns the library read error.
pub async fn stored_taste_axes(
    library: &LibraryStore,
    subject: &DiscoverSubject,
) -> Result<Option<StoredAxes>> {
    let Some(row) = library.get_discover_taste(subject).await? else {
        return Ok(None);
    };
    Ok(
        StoredTaste::decode(&row.taste_json).map(|taste| StoredAxes {
            computed_at: row.computed_at,
            axes: taste.scores.axes,
        }),
    )
}

/// `vector` scaled to length 1; `None` when it has no direction.
fn unit(vector: &[f32]) -> Option<Vec<f32>> {
    let norm = vector
        .iter()
        .map(|x| f64::from(*x) * f64::from(*x))
        .sum::<f64>()
        .sqrt();
    if norm <= 0.0 || !norm.is_finite() {
        return None;
    }
    // Each number divided by the length is within -1..=1, which f32 holds.
    #[allow(clippy::cast_possible_truncation)]
    Some(
        vector
            .iter()
            .map(|x| (f64::from(*x) / norm) as f32)
            .collect(),
    )
}

/// The dot product of two vectors of one length.
fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// Where an axis points: the weighted mean of its members' vectors, each
/// first scaled to length 1 so a long vector counts no more than a short
/// one. Not unit length itself.
///
/// # Arguments
///
/// * `members` - The works on the axis.
/// * `vector_of` - The vector of a work in the space being read, when it
///   has one. Members without one, or with a vector of another length
///   than the first found, are left out.
///
/// # Returns
///
/// `None` when no member has a vector.
pub fn axis_centroid<'a>(
    members: &[AxisMember],
    vector_of: impl Fn(&str) -> Option<&'a [f32]>,
) -> Option<Vec<f32>> {
    let mut sum: Option<Vec<f64>> = None;
    let mut total = 0.0_f64;
    for member in members {
        let Some(vector) = vector_of(&member.work_id).and_then(unit) else {
            continue;
        };
        let weight = member.weight.max(0.0);
        match &mut sum {
            Some(acc) if acc.len() == vector.len() => {
                for (into, x) in acc.iter_mut().zip(&vector) {
                    *into += weight * f64::from(*x);
                }
                total += weight;
            }
            Some(_) => {}
            None => {
                sum = Some(vector.iter().map(|x| weight * f64::from(*x)).collect());
                total = weight;
            }
        }
    }
    let sum = sum.filter(|_| total > 0.0)?;
    // A mean of numbers within -1..=1.
    #[allow(clippy::cast_possible_truncation)]
    Some(sum.into_iter().map(|x| (x / total) as f32).collect())
}

/// The narrowest band between an axis's own works and the person's other
/// works that a fit is measured across. Two axes whose works are equally
/// close to a centroid would otherwise divide by nothing.
const AXIS_MIN_BAND: f32 = 0.02;

/// How much the second and the third best axis of a book count beside its
/// best: half, then a quarter. A fourth adds nothing.
const AXIS_SHARES: [f32; 3] = [1.0, 0.5, 0.25];

/// One axis as a book is measured against it.
///
/// A model's vectors all point roughly the same way, so a cosine of 0.8
/// says nothing by itself. An axis therefore carries its own scale: how
/// close its own works sit to where it points, and how close the works of
/// the person's other axes do. A book's fit is where it lands between the
/// two.
#[derive(Debug, Clone, PartialEq)]
pub struct AxisScale {
    /// Where the axis points ([`axis_centroid`]).
    pub centroid: Vec<f32>,
    /// Weighted mean cosine of the axis's own works to `centroid`: a book
    /// this close fits the axis in full.
    pub near: f32,
    /// Weighted mean cosine of the other axes' works to `centroid`: a book
    /// no closer than this does not fit the axis at all.
    pub far: f32,
}

impl AxisScale {
    /// The cosine of `vector` to where the axis points; `None` when it is
    /// of another length.
    #[must_use]
    pub fn closeness(&self, vector: &[f32]) -> Option<f32> {
        (vector.len() == self.centroid.len()).then(|| crate::embed::cosine(&self.centroid, vector))
    }

    /// How well a book at cosine `closeness` to the axis fits it: 0 when it
    /// is no closer than the person's other works, 1 when it is as close as
    /// the axis's own.
    #[must_use]
    pub fn fit(&self, closeness: f32) -> f32 {
        let band = (self.near - self.far).max(AXIS_MIN_BAND);
        let fit = (closeness - self.far) / band;
        if fit.is_finite() {
            fit.clamp(0.0, 1.0)
        } else {
            0.0
        }
    }
}

/// The weighted mean cosine of `members`' vectors to `centroid`; `None`
/// when none has a vector of its length or a weight.
fn mean_closeness<'a>(
    centroid: &[f32],
    members: impl Iterator<Item = &'a AxisMember>,
    vector_of: impl Fn(&str) -> Option<&'a [f32]>,
) -> Option<f32> {
    let mut sum = 0.0_f64;
    let mut total = 0.0_f64;
    for member in members {
        let Some(vector) = vector_of(&member.work_id).filter(|v| v.len() == centroid.len()) else {
            continue;
        };
        let weight = member.weight.max(0.0);
        sum += weight * f64::from(crate::embed::cosine(centroid, vector));
        total += weight;
    }
    // A mean of cosines, within -1..=1.
    #[allow(clippy::cast_possible_truncation)]
    (total > 0.0).then(|| (sum / total) as f32)
}

/// The scale of each axis in the space `vector_of` reads.
///
/// # Arguments
///
/// * `axes` - A taste's stored axes.
/// * `vector_of` - The vector of a work, when it has one in that space.
///
/// # Returns
///
/// One entry for each axis, in order; `None` for an axis none of whose
/// works has a vector.
pub fn axis_scales<'a>(
    axes: &'a [TasteAxis],
    vector_of: impl Fn(&str) -> Option<&'a [f32]> + Copy,
) -> Vec<Option<AxisScale>> {
    axes.iter()
        .enumerate()
        .map(|(at, axis)| {
            let centroid = axis_centroid(&axis.members, vector_of)?;
            let near = mean_closeness(&centroid, axis.members.iter(), vector_of)?;
            let others = axes
                .iter()
                .enumerate()
                .filter(|(other, _)| *other != at)
                .flat_map(|(_, other)| other.members.iter());
            let far = mean_closeness(&centroid, others, vector_of)
                .unwrap_or(near - AXIS_MIN_BAND)
                .min(near - AXIS_MIN_BAND);
            Some(AxisScale {
                centroid,
                near,
                far,
            })
        })
        .collect()
}

/// How well a book fits a whole taste, from its fit on each axis: the best
/// axis in full, the second at half and the third at a quarter, so a book
/// near one axis ranks high and a book near several ranks higher.
///
/// # Returns
///
/// 0 to 1.75.
#[must_use]
pub fn taste_fit(fits: impl IntoIterator<Item = f32>) -> f32 {
    let mut fits: Vec<f32> = fits.into_iter().filter(|fit| fit.is_finite()).collect();
    fits.sort_by(|a, b| b.total_cmp(a));
    fits.iter()
        .zip(AXIS_SHARES)
        .map(|(fit, share)| fit.clamp(0.0, 1.0) * share)
        .sum()
}

/// One seed work as it is grouped.
#[derive(Debug, Clone)]
pub struct AxisSeed {
    /// The work.
    pub work_id: String,
    /// How much the work counts in the taste.
    pub weight: f64,
    /// The work's vector, any length above zero.
    pub vector: Vec<f32>,
}

/// How many groups `seeds` works are first split into: the square root of
/// their number, within [`MIN_TASTE_AXES`] and [`MAX_TASTE_AXES`], so a
/// dozen works are not cut into six pairs.
fn group_count(seeds: usize) -> usize {
    let mut k = MIN_TASTE_AXES;
    while k < MAX_TASTE_AXES && (k + 1) * (k + 1) <= seeds {
        k += 1;
    }
    k
}

/// Splits the seed works of a taste into groups that sit together in the
/// embedding space.
///
/// Spherical k-means with nothing left to chance, so the same taste gives
/// the same groups: the first centre is the heaviest work, each next
/// centre the work least like the centres so far, and works move to the
/// centre they are most like until none moves. A group left with fewer
/// than [`AXIS_MIN_WORKS`] works is dissolved into the others.
///
/// # Arguments
///
/// * `seeds` - The seed works. Only the [`MAX_AXIS_SEEDS`] heaviest are
///   grouped; a work without a direction, or whose vector differs in
///   length from the heaviest work's, is left out.
///
/// # Returns
///
/// The groups as positions in `seeds`, the group with the most weight
/// first and each group's heaviest work first. Empty when fewer than
/// [`MIN_TASTE_AXES`] groups hold.
#[must_use]
pub fn cluster(seeds: &[AxisSeed]) -> Vec<Vec<usize>> {
    let mut order: Vec<usize> = (0..seeds.len()).collect();
    order.sort_by(|a, b| {
        seeds[*b]
            .weight
            .total_cmp(&seeds[*a].weight)
            .then_with(|| seeds[*a].work_id.cmp(&seeds[*b].work_id))
    });
    order.truncate(MAX_AXIS_SEEDS);
    let dims = order.first().map_or(0, |first| seeds[*first].vector.len());
    // (position in `seeds`, unit vector), heaviest first.
    let points: Vec<(usize, Vec<f32>)> = order
        .into_iter()
        .filter(|position| seeds[*position].vector.len() == dims)
        .filter_map(|position| Some((position, unit(&seeds[position].vector)?)))
        .collect();
    if points.len() < MIN_TASTE_AXES * AXIS_MIN_WORKS {
        return Vec::new();
    }
    let k = group_count(points.len());
    // Farthest-point start: the heaviest work, then the work least like
    // every centre so far (the first such when several are).
    let mut centres: Vec<Vec<f32>> = vec![points[0].1.clone()];
    while centres.len() < k {
        let mut farthest = (0_usize, f32::INFINITY);
        for (at, (_, vector)) in points.iter().enumerate() {
            let nearest = centres
                .iter()
                .map(|centre| dot(centre, vector))
                .fold(f32::NEG_INFINITY, f32::max);
            if nearest < farthest.1 {
                farthest = (at, nearest);
            }
        }
        centres.push(points[farthest.0].1.clone());
    }
    let nearest_centre = |centres: &[Option<Vec<f32>>], vector: &[f32]| {
        let mut best = (0_usize, f32::NEG_INFINITY);
        for (at, centre) in centres.iter().enumerate() {
            let Some(centre) = centre else { continue };
            let likeness = dot(centre, vector);
            if likeness > best.1 {
                best = (at, likeness);
            }
        }
        best.0
    };
    let mut centres: Vec<Option<Vec<f32>>> = centres.into_iter().map(Some).collect();
    let mut assigned: Vec<usize> = vec![usize::MAX; points.len()];
    loop {
        for _ in 0..CLUSTER_ROUNDS {
            let mut moved = false;
            for (at, (_, vector)) in points.iter().enumerate() {
                let to = nearest_centre(&centres, vector);
                if assigned[at] != to {
                    assigned[at] = to;
                    moved = true;
                }
            }
            for (group, centre) in centres.iter_mut().enumerate() {
                if centre.is_none() {
                    continue;
                }
                let mut sum = vec![0.0_f64; dims];
                for (at, (position, vector)) in points.iter().enumerate() {
                    if assigned[at] == group {
                        let weight = seeds[*position].weight.max(0.0);
                        for (into, x) in sum.iter_mut().zip(vector) {
                            *into += weight * f64::from(*x);
                        }
                    }
                }
                // Narrowing sums of numbers within the works' weights.
                #[allow(clippy::cast_possible_truncation)]
                let mean: Vec<f32> = sum.into_iter().map(|x| x as f32).collect();
                // A group all of whose works weigh nothing keeps its centre.
                if let Some(mean) = unit(&mean) {
                    *centre = Some(mean);
                }
            }
            if !moved {
                break;
            }
        }
        // Dissolve the smallest group that is too small, then settle again.
        let sizes: Vec<usize> = (0..centres.len())
            .map(|group| assigned.iter().filter(|to| **to == group).count())
            .collect();
        let too_small = (0..centres.len())
            .filter(|group| centres[*group].is_some() && sizes[*group] < AXIS_MIN_WORKS)
            .min_by_key(|group| sizes[*group]);
        match too_small {
            Some(group) => centres[group] = None,
            None => break,
        }
        if centres.iter().flatten().count() < MIN_TASTE_AXES {
            return Vec::new();
        }
    }
    let mut groups: Vec<(f64, Vec<usize>)> = (0..centres.len())
        .filter(|group| centres[*group].is_some())
        .map(|group| {
            let members: Vec<usize> = points
                .iter()
                .enumerate()
                .filter(|(at, _)| assigned[*at] == group)
                .map(|(_, (position, _))| *position)
                .collect();
            let weight = members.iter().map(|m| seeds[*m].weight.max(0.0)).sum();
            (weight, members)
        })
        .collect();
    if groups.len() < MIN_TASTE_AXES {
        return Vec::new();
    }
    // A stable sort: equal weights keep the order of their centres.
    groups.sort_by(|a, b| b.0.total_cmp(&a.0));
    groups.into_iter().map(|(_, members)| members).collect()
}

/// What is known of a work when axes are named.
#[derive(Debug, Clone, Default)]
pub struct AxisBook {
    /// Display title.
    pub title: String,
    /// Genres, categories or subjects, as one line of labels.
    pub genres: Option<String>,
    /// Names of the series the work belongs to.
    pub series: Vec<String>,
    /// Synopsis; may contain HTML.
    pub description: Option<String>,
}

impl AxisBook {
    /// What naming reads of a library work.
    #[must_use]
    pub fn of(work: &WorkRecord) -> Self {
        Self {
            title: work.title.clone(),
            genres: work.categories.clone().or_else(|| work.subjects.clone()),
            series: work.series.clone(),
            description: work.description.clone(),
        }
    }
}

/// The words of the start of a description, without its markup.
fn description_words(description: &str) -> Vec<String> {
    let mut text = String::new();
    let mut in_tag = false;
    for c in description.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => {
                in_tag = false;
                text.push(' ');
            }
            _ if in_tag => {}
            _ => text.push(c),
        }
        if text.chars().count() >= PHRASE_DESCRIPTION_CHARS {
            break;
        }
    }
    text.split(|c: char| !(c.is_alphanumeric() || c == '\'' || c == '’'))
        .map(|word| word.trim_matches(['\'', '’']))
        .filter(|word| !word.is_empty())
        .map(str::to_string)
        .collect()
}

/// The phrases of two to four words in a description that neither start
/// nor end with a stopword, each once, as (phrase as written, lowercase
/// key).
fn description_phrases(description: &str) -> Vec<(String, String)> {
    let words = description_words(description);
    let lower: Vec<String> = words.iter().map(|word| word.to_lowercase()).collect();
    let plain = |word: &str| STOPWORDS.contains(&word) || word.chars().all(char::is_numeric);
    let mut phrases: Vec<(String, String)> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for len in 2..=4 {
        for start in 0..words.len().saturating_sub(len - 1) {
            let end = start + len;
            if plain(&lower[start]) || plain(&lower[end - 1]) {
                continue;
            }
            let key = lower[start..end].join(" ");
            if seen.insert(key.clone()) {
                phrases.push((words[start..end].join(" "), key));
            }
        }
    }
    phrases
}

/// A name an axis could take.
struct Candidate {
    /// The name as first written.
    display: String,
    /// Whether it is a description phrase (not a genre or a series).
    phrase: bool,
    /// Works carrying it, by the group they are in.
    works: Vec<usize>,
    /// Works carrying it in all.
    total: usize,
}

/// Whether `part` is at least the share `of` (numerator, denominator) of
/// `whole`.
fn at_least(part: usize, whole: usize, of: (usize, usize)) -> bool {
    part.saturating_mul(of.1) >= whole.saturating_mul(of.0)
}

/// `count` as a float; the counts here are of works, far below the
/// integers f64 holds exactly.
fn count(count: usize) -> f64 {
    f64::from(u32::try_from(count).unwrap_or(u32::MAX))
}

/// Names each group of a taste by what its works share and the person's
/// other works lack.
///
/// A name is a genre label, a series, or a phrase of two to four words
/// from the works' descriptions. Its distinctiveness is the share of the
/// group's works that carry it less the share of the other groups' works
/// that do; the most distinctive names the group, a genre or series
/// winning over a phrase unless the phrase is clearly more distinctive
/// ([`PHRASE_HANDICAP`]), and a longer phrase over the shorter one inside
/// it. A group with nothing distinctive enough is named after its first
/// work, "Like *title*". No two groups share a name.
///
/// # Arguments
///
/// * `groups` - The groups, each a list of positions in `books`, the
///   work that best stands for the group first.
/// * `books` - What is known of each work.
///
/// # Returns
///
/// One name for each group, in the order of `groups`.
#[must_use]
pub fn name_axes(groups: &[Vec<usize>], books: &[AxisBook]) -> Vec<String> {
    let mut candidates: Vec<Candidate> = Vec::new();
    let mut by_key: HashMap<(bool, String), usize> = HashMap::new();
    for (group, members) in groups.iter().enumerate() {
        for member in members {
            let Some(book) = books.get(*member) else {
                continue;
            };
            let mut found: Vec<(bool, String, String)> = Vec::new();
            for (label, key) in book.genres.as_deref().map(genre_labels).unwrap_or_default() {
                found.push((false, label, key));
            }
            for series in &book.series {
                let key = series.trim().to_lowercase();
                if !key.is_empty() && !found.iter().any(|(_, _, seen)| *seen == key) {
                    found.push((false, series.trim().to_string(), key));
                }
            }
            for (phrase, key) in book
                .description
                .as_deref()
                .map(description_phrases)
                .unwrap_or_default()
            {
                found.push((true, phrase, key));
            }
            for (phrase, display, key) in found {
                let at = *by_key.entry((phrase, key)).or_insert_with(|| {
                    candidates.push(Candidate {
                        display,
                        phrase,
                        works: vec![0; groups.len()],
                        total: 0,
                    });
                    candidates.len() - 1
                });
                candidates[at].works[group] += 1;
                candidates[at].total += 1;
            }
        }
    }
    let everyone: usize = groups.iter().map(Vec::len).sum();
    let mut taken: Vec<String> = Vec::new();
    let mut names = Vec::with_capacity(groups.len());
    for (group, members) in groups.iter().enumerate() {
        let others = everyone - members.len();
        // (score, words, display): the best so far.
        let mut best: Option<(f64, usize, &str)> = None;
        for candidate in &candidates {
            let inside = candidate.works[group];
            let share = if candidate.phrase {
                PHRASE_MIN_SHARE
            } else {
                LABEL_MIN_SHARE
            };
            if inside < AXIS_MIN_WORKS
                || !at_least(inside, members.len(), share)
                || taken.contains(&candidate.display.to_lowercase())
            {
                continue;
            }
            let outside = candidate.total - inside;
            let distinct = count(inside) / count(members.len())
                - if others == 0 {
                    0.0
                } else {
                    count(outside) / count(others)
                };
            if distinct < NAME_MIN_DISTINCT {
                continue;
            }
            let score = distinct
                - if candidate.phrase {
                    PHRASE_HANDICAP
                } else {
                    0.0
                };
            let words = candidate.display.split_whitespace().count();
            let better = best.is_none_or(|(top, top_words, top_display)| {
                score > top + 1e-9
                    || ((score - top).abs() <= 1e-9
                        && (words > top_words
                            || (words == top_words && candidate.display.as_str() < top_display)))
            });
            if better {
                best = Some((score, words, candidate.display.as_str()));
            }
        }
        let name = match best {
            Some((_, _, display)) => display.to_string(),
            None => {
                let title = members
                    .first()
                    .and_then(|member| books.get(*member))
                    .map_or("", |book| book.title.trim());
                format!("Like {title}")
            }
        };
        taken.push(name.to_lowercase());
        names.push(name);
    }
    names
}

/// Builds the axes of a taste from its seed works' weights.
///
/// # Arguments
///
/// * `library` - The library store.
/// * `space` - The model's space whose stored work vectors are grouped.
/// * `weights` - Each seed work and how much it counts.
///
/// # Returns
///
/// The axes, the one with the most weight first; none when too few seed
/// works have a vector or sit together.
///
/// # Errors
///
/// Returns the library read error.
pub(crate) async fn build_axes(
    library: &LibraryStore,
    space: &VectorSpace,
    weights: &BTreeMap<String, f64>,
) -> Result<Vec<TasteAxis>> {
    let mut heaviest: Vec<(&String, f64)> = weights
        .iter()
        .filter(|(_, weight)| **weight > 0.0)
        .map(|(id, weight)| (id, *weight))
        .collect();
    heaviest.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    heaviest.truncate(MAX_AXIS_SEEDS);
    if heaviest.len() < MIN_TASTE_AXES * AXIS_MIN_WORKS {
        return Ok(Vec::new());
    }
    let ids: Vec<String> = heaviest.iter().map(|(id, _)| (*id).clone()).collect();
    let stored = library
        .vectors(bookclerk_library::VECTOR_KIND_WORK, space, &ids)
        .await?;
    let seeds: Vec<AxisSeed> = heaviest
        .into_iter()
        .filter_map(|(id, weight)| {
            Some(AxisSeed {
                work_id: id.clone(),
                weight,
                vector: bytes_to_vector(stored.get(id)?),
            })
        })
        .collect();
    let groups = cluster(&seeds);
    if groups.is_empty() {
        return Ok(Vec::new());
    }
    let grouped: Vec<String> = groups
        .iter()
        .flatten()
        .map(|position| seeds[*position].work_id.clone())
        .collect();
    let works = library.works_by_ids(&grouped).await?;
    let books: Vec<AxisBook> = seeds
        .iter()
        .map(|seed| {
            works
                .get(&seed.work_id)
                .map(AxisBook::of)
                .unwrap_or_default()
        })
        .collect();
    // A group is named after its first work when nothing else stands
    // out: put the work nearest where the group points first.
    let groups: Vec<Vec<usize>> = groups
        .into_iter()
        .map(|mut members| {
            let of_group: Vec<AxisMember> = members
                .iter()
                .map(|m| AxisMember {
                    work_id: seeds[*m].work_id.clone(),
                    weight: seeds[*m].weight,
                })
                .collect();
            let by_id: HashMap<&str, &[f32]> = members
                .iter()
                .map(|m| (seeds[*m].work_id.as_str(), seeds[*m].vector.as_slice()))
                .collect();
            if let Some(centre) = axis_centroid(&of_group, |id| by_id.get(id).copied()) {
                let likeness = |m: &usize| crate::embed::cosine(&centre, &seeds[*m].vector);
                if let Some(nearest) = (0..members.len()).max_by(|a, b| {
                    likeness(&members[*a])
                        .total_cmp(&likeness(&members[*b]))
                        .then_with(|| b.cmp(a))
                }) {
                    members.swap(0, nearest);
                }
            }
            members
        })
        .collect();
    let names = name_axes(&groups, &books);
    Ok(groups
        .into_iter()
        .zip(names)
        .map(|(members, name)| {
            let mut members: Vec<AxisMember> = members
                .into_iter()
                .map(|m| AxisMember {
                    work_id: seeds[m].work_id.clone(),
                    weight: seeds[m].weight,
                })
                .collect();
            members.sort_by(|a, b| {
                b.weight
                    .total_cmp(&a.weight)
                    .then_with(|| a.work_id.cmp(&b.work_id))
            });
            TasteAxis { name, members }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A seed pointing along `vector`.
    fn seed(work_id: &str, weight: f64, vector: [f32; 3]) -> AxisSeed {
        AxisSeed {
            work_id: work_id.to_string(),
            weight,
            vector: vector.to_vec(),
        }
    }

    /// The work ids of each group, sorted within the group.
    fn ids(seeds: &[AxisSeed], groups: &[Vec<usize>]) -> Vec<Vec<String>> {
        groups
            .iter()
            .map(|group| {
                let mut ids: Vec<String> =
                    group.iter().map(|m| seeds[*m].work_id.clone()).collect();
                ids.sort();
                ids
            })
            .collect()
    }

    /// Nine works in three tight bunches along the three axes.
    fn three_bunches() -> Vec<AxisSeed> {
        vec![
            seed("x1", 3.0, [1.0, 0.05, 0.0]),
            seed("x2", 1.0, [1.0, 0.0, 0.05]),
            seed("x3", 1.0, [0.9, 0.1, 0.0]),
            seed("y1", 2.0, [0.0, 1.0, 0.05]),
            seed("y2", 2.0, [0.05, 1.0, 0.0]),
            seed("y3", 2.0, [0.0, 0.9, 0.1]),
            seed("z1", 0.5, [0.0, 0.05, 1.0]),
            seed("z2", 0.5, [0.05, 0.0, 1.0]),
            seed("z3", 0.5, [0.1, 0.0, 0.9]),
        ]
    }

    #[test]
    fn works_that_sit_together_make_a_group_heaviest_group_first() {
        let seeds = three_bunches();
        let groups = cluster(&seeds);
        assert_eq!(
            ids(&seeds, &groups),
            [["y1", "y2", "y3"], ["x1", "x2", "x3"], ["z1", "z2", "z3"]],
            "by weight: 6, then 5, then 1.5"
        );
        // The same taste in another order gives the same groups.
        let mut shuffled = seeds.clone();
        shuffled.reverse();
        assert_eq!(ids(&shuffled, &cluster(&shuffled)), ids(&seeds, &groups));
    }

    #[test]
    fn too_few_works_or_groups_make_no_axes() {
        let seeds = three_bunches();
        assert!(
            cluster(&seeds[..5]).is_empty(),
            "three pairs need six works"
        );
        assert!(cluster(&[]).is_empty());
        // Two bunches and a loner: the loner's group dissolves, two are left.
        let mut two: Vec<AxisSeed> = seeds[..6].to_vec();
        two.push(seed("z1", 0.5, [0.0, 0.0, 1.0]));
        assert!(cluster(&two).is_empty());
    }

    #[test]
    fn works_without_a_direction_or_of_another_length_are_left_out() {
        let mut seeds = three_bunches();
        seeds.push(seed("zero", 9.0, [0.0, 0.0, 0.0]));
        seeds.push(AxisSeed {
            work_id: "short".into(),
            weight: 0.1,
            vector: vec![1.0, 0.0],
        });
        let groups = cluster(&seeds);
        assert_eq!(groups.len(), 3);
        assert!(ids(&seeds, &groups)
            .iter()
            .flatten()
            .all(|id| id != "zero" && id != "short"));
    }

    #[test]
    fn a_dozen_works_are_not_cut_into_six_pairs() {
        assert_eq!(group_count(6), 3);
        assert_eq!(group_count(15), 3);
        assert_eq!(group_count(16), 4);
        assert_eq!(group_count(25), 5);
        assert_eq!(group_count(36), 6);
        assert_eq!(group_count(4000), 6);
    }

    #[test]
    fn a_centroid_is_the_weighted_mean_of_unit_vectors() {
        let members = [
            AxisMember {
                work_id: "a".into(),
                weight: 3.0,
            },
            AxisMember {
                work_id: "b".into(),
                weight: 1.0,
            },
            AxisMember {
                work_id: "none".into(),
                weight: 5.0,
            },
        ];
        let a = [2.0_f32, 0.0];
        let b = [0.0_f32, 0.5];
        let centroid = axis_centroid(&members, |id| match id {
            "a" => Some(&a[..]),
            "b" => Some(&b[..]),
            _ => None,
        })
        .unwrap();
        assert_eq!(centroid, [0.75, 0.25], "length does not count, weight does");
        assert!(axis_centroid(&members[2..], |_| None).is_none());
    }

    #[test]
    fn a_fit_runs_from_the_other_axes_works_to_the_axis_own() {
        let axis = |name: &str, ids: [&str; 2]| TasteAxis {
            name: name.into(),
            members: ids
                .iter()
                .map(|id| AxisMember {
                    work_id: (*id).into(),
                    weight: 1.0,
                })
                .collect(),
        };
        let axes = [
            axis("x", ["x1", "x2"]),
            axis("y", ["y1", "y2"]),
            axis("gone", ["g1", "g2"]),
        ];
        let x = [1.0_f32, 0.0];
        let y = [0.0_f32, 1.0];
        let scales = axis_scales(&axes, |id| match id {
            "x1" | "x2" => Some(&x[..]),
            "y1" | "y2" => Some(&y[..]),
            _ => None,
        });
        assert!(scales[2].is_none(), "no work of the axis has a vector");
        let scale = scales[0].as_ref().unwrap();
        assert_eq!((scale.near, scale.far), (1.0, 0.0));
        assert_eq!(scale.fit(scale.closeness(&x).unwrap()), 1.0);
        assert_eq!(scale.fit(scale.closeness(&y).unwrap()), 0.0);
        let between = scale.fit(scale.closeness(&[1.0, 1.0]).unwrap());
        assert!((between - 0.707).abs() < 0.01, "{between}");
        assert_eq!(scale.fit(-0.5), 0.0, "never below nothing");
        assert_eq!(scale.closeness(&[1.0, 0.0, 0.0]), None);
        // Works equally close to both ends still give a band to measure in.
        let flat = AxisScale {
            centroid: vec![1.0, 0.0],
            near: 0.8,
            far: 0.8,
        };
        assert_eq!(flat.fit(0.8), 0.0);
        assert_eq!(flat.fit(0.9), 1.0);
    }

    #[test]
    fn a_book_near_several_axes_fits_better_than_a_book_near_one() {
        assert_eq!(taste_fit([0.2, 1.0, 0.0]), 1.1);
        assert_eq!(taste_fit([1.0]), 1.0);
        assert_eq!(taste_fit([1.0, 1.0, 1.0, 1.0, 1.0]), 1.75);
        assert_eq!(taste_fit([]), 0.0);
        assert!(taste_fit([0.8, 0.8]) > taste_fit([1.0]));
    }

    /// A book with `genres`, in `series`, described by `description`.
    fn book(title: &str, genres: &str, series: &[&str], description: &str) -> AxisBook {
        AxisBook {
            title: title.to_string(),
            genres: (!genres.is_empty()).then(|| genres.to_string()),
            series: series.iter().map(|s| (*s).to_string()).collect(),
            description: (!description.is_empty()).then(|| description.to_string()),
        }
    }

    #[test]
    fn a_group_is_named_by_what_its_works_share_and_the_others_lack() {
        let books = [
            book("Dune", "Fiction, Science Fiction", &[], ""),
            book("Hyperion", "Science Fiction, Classics", &[], ""),
            book("Guards! Guards!", "Fiction, Fantasy", &["Discworld"], ""),
            book("Mort", "Fantasy, Humor", &["Discworld"], ""),
            book(
                "Educated",
                "Biography",
                &[],
                "<p>The #1 <b>New York Times</b> bestseller about a girl.</p>",
            ),
            book(
                "Becoming",
                "Memoir",
                &[],
                "A New York Times bestseller, and the story of a life.",
            ),
            book("Loner", "", &[], "Nothing in common."),
            book("Other", "", &[], "With anything else at all."),
        ];
        let groups = vec![vec![0, 1], vec![2, 3], vec![4, 5], vec![6, 7]];
        assert_eq!(
            name_axes(&groups, &books),
            [
                "Science Fiction",
                "Discworld",
                "New York Times bestseller",
                "Like Loner"
            ],
            "a genre; a series before the genre beside it by name; the longest \
             shared phrase; and the first work when nothing is shared"
        );
    }

    #[test]
    fn a_label_everyone_carries_names_nothing_and_no_name_is_used_twice() {
        let books = [
            book("A", "Fantasy", &[], ""),
            book("B", "Fantasy", &[], ""),
            book("C", "Fantasy", &[], ""),
            book("D", "Fantasy", &[], ""),
        ];
        let groups = vec![vec![0, 1], vec![2, 3]];
        assert_eq!(name_axes(&groups, &books), ["Like A", "Like C"]);
        // Shared by one group and half of the other: still the first's name
        // (1.0 - 0.5), and the other falls back.
        let books = [
            book("A", "Noir", &[], ""),
            book("B", "Noir", &[], ""),
            book("C", "Noir", &[], ""),
            book("D", "", &[], ""),
        ];
        assert_eq!(name_axes(&groups, &books), ["Noir", "Like C"]);
    }

    #[test]
    fn phrases_skip_markup_stopword_ends_and_numbers() {
        let phrases: Vec<String> =
            description_phrases("<i>The</i> winner of the 2019 Hugo Award, at last")
                .into_iter()
                .map(|(phrase, _)| phrase)
                .collect();
        assert!(phrases.contains(&"Hugo Award".to_string()));
        assert!(phrases.iter().all(|p| !p.ends_with("2019")));
        assert!(phrases.iter().all(|p| !p.starts_with("The ")));
        assert!(phrases.iter().all(|p| !p.ends_with(" the")));
        assert!(phrases.iter().all(|p| !p.contains('<')));
    }
}
