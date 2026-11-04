//! The resolver: one listing and a snapshot in, one plan of writes out.
//!
//! The steps follow the library data model ADR:
//!
//! 1. **Edition.** An Administrator's pin wins. A listing seen before keeps
//!    its edition. Otherwise a strong identifier names the edition, and
//!    failing that a weak identifier (an ISBN) proposes one that is accepted
//!    only when its media kind, work kind and expression are compatible and
//!    it holds no different value for one of the listing's strong schemes.
//!    Strong identifiers that name different editions are never merged; the
//!    conflict is recorded.
//! 2. **Work**, for a new edition: a shared work identifier, else the work
//!    matcher (cleaned title and authors, same kind, and for an episode the
//!    same parent show), else a new work. Two matching works means the
//!    listing joins neither and the conflict is recorded.
//! 3. **Expression** within the work: same kind, known and equal language
//!    and abridgement, and equal performer credits. Unknown values never
//!    match, so missing data makes a new expression rather than a wrong merge.

use std::collections::BTreeSet;

use crate::identifiers::IdentifierKey;
use crate::kinds::{Abridgement, ExpressionKind, Level, Strength, WorkKind};
use crate::listing::{HoldingDraft, Listing};
use crate::model::{
    Conflict, CreditWrite, EditionIdentifierRow, EditionRow, Event, ExpressionRow, HoldingRow,
    LinkReason, ListingRow, Override, Plan, SeriesWrite, Snapshot, WorkIdentifierRow, WorkParent,
    WorkReason, WorkRow,
};

/// The kind of row a new id is minted for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntityKind {
    /// A work.
    Work,
    /// An expression.
    Expression,
    /// An edition.
    Edition,
    /// A store listing.
    Listing,
}

/// Mints ids for new rows; the store uses random UUIDs, tests use counters.
pub trait IdSource {
    /// A fresh id, never returned before, for a new row of `kind`.
    fn next_id(&mut self, kind: EntityKind) -> String;
}

/// Whether a stored expression may take `listing`.
///
/// Kinds must be equal; language and abridgement must be known on both
/// sides and equal; the performer credit keys must be equal and, except
/// for the text itself, non-empty.
fn expression_compatible(expression: &ExpressionRow, listing: &Listing) -> bool {
    let credit_key = listing.expression_credit_key();
    expression.kind == listing.expression_kind
        && matches!(
            (&expression.language, &listing.language),
            (Some(a), Some(b)) if a == b
        )
        && expression.abridgement != Abridgement::Unknown
        && expression.abridgement == listing.abridgement
        && expression.credit_key == credit_key
        && (listing.expression_kind == ExpressionKind::Text || !credit_key.is_empty())
}

/// Whether `edition_id` holds a different value for one of the listing's
/// strong identifier schemes in the same marketplace.
fn strong_clash(snapshot: &Snapshot, edition_id: &str, listing: &Listing) -> bool {
    listing
        .edition_identifiers()
        .filter(|key| key.strength() == Strength::Strong)
        .any(|key| clashes(snapshot, edition_id, key))
}

/// Whether `edition_id` holds another value of `key`'s scheme and marketplace.
fn clashes(snapshot: &Snapshot, edition_id: &str, key: &IdentifierKey) -> bool {
    snapshot.edition_identifiers.iter().any(|row| {
        row.edition_id == edition_id
            && row.key.scheme == key.scheme
            && row.key.marketplace == key.marketplace
            && row.key.value != key.value
    })
}

/// Included editions holding `key`, in id order.
fn owners<'a>(snapshot: &'a Snapshot, key: &IdentifierKey) -> Vec<&'a str> {
    let set: BTreeSet<&str> = snapshot
        .edition_identifiers
        .iter()
        .filter(|row| &row.key == key && snapshot.edition(&row.edition_id).is_some())
        .map(|row| row.edition_id.as_str())
        .collect();
    set.into_iter().collect()
}

/// Included works holding the work identifier `key`, in id order.
fn work_owners<'a>(snapshot: &'a Snapshot, key: &IdentifierKey) -> Vec<&'a str> {
    let set: BTreeSet<&str> = snapshot
        .work_identifiers
        .iter()
        .filter(|row| &row.key == key && snapshot.work(&row.work_id).is_some())
        .map(|row| row.work_id.as_str())
        .collect();
    set.into_iter().collect()
}

/// Pushes `items` onto `out` in order, skipping ones already there.
fn push_unique(out: &mut Vec<String>, items: impl IntoIterator<Item = String>) {
    for item in items {
        if !out.contains(&item) {
            out.push(item);
        }
    }
}

/// The edition a weak identifier proposes and the listing may join.
fn weak_candidate(snapshot: &Snapshot, listing: &Listing) -> Option<String> {
    for key in listing
        .edition_identifiers()
        .filter(|key| key.strength() == Strength::Weak)
    {
        for edition_id in owners(snapshot, key) {
            let Some(edition) = snapshot.edition(edition_id) else {
                continue;
            };
            let Some(expression) = snapshot.expression(&edition.expression_id) else {
                continue;
            };
            let Some(work) = snapshot.work(&expression.work_id) else {
                continue;
            };
            if edition.media_kind == listing.media_kind
                && work.kind == listing.work_kind
                && expression_compatible(expression, listing)
                && !strong_clash(snapshot, edition_id, listing)
            {
                return Some(edition_id.to_string());
            }
        }
    }
    None
}

/// The work a new edition joins, and how it was found.
fn find_work(
    snapshot: &Snapshot,
    listing: &Listing,
    parent_work_id: Option<&str>,
    conflicts: &mut Vec<Conflict>,
) -> Option<(WorkRow, WorkReason)> {
    let mut by_identifier = Vec::new();
    for key in listing.work_identifiers() {
        push_unique(
            &mut by_identifier,
            work_owners(snapshot, key).into_iter().map(str::to_string),
        );
    }
    if let Some(first) = by_identifier.first().and_then(|id| snapshot.work(id)) {
        if by_identifier.len() > 1 {
            conflicts.push(Conflict::WorkIdentifiers {
                work_ids: by_identifier.clone(),
            });
        }
        if first.kind == listing.work_kind {
            return Some((first.clone(), WorkReason::Identifier));
        }
        conflicts.push(Conflict::WorkKindMismatch {
            work_id: first.id.clone(),
        });
    }

    let key = listing.work_match_key();
    if key.is_empty() {
        return None;
    }
    let isolated: BTreeSet<&str> = snapshot
        .overrides
        .iter()
        .filter_map(|o| match o {
            Override::IsolateWork { work_id } => Some(work_id.as_str()),
            Override::PinListing { .. } => None,
        })
        .collect();
    let mut matches: Vec<&WorkRow> = snapshot
        .works
        .iter()
        .filter(|w| {
            w.match_key == key
                && w.kind == listing.work_kind
                && !isolated.contains(w.id.as_str())
                && match listing.work_kind {
                    WorkKind::PodcastEpisode => {
                        parent_work_id.is_some() && w.parent_work_id.as_deref() == parent_work_id
                    }
                    WorkKind::Book | WorkKind::PodcastShow | WorkKind::Drama => true,
                }
        })
        .collect();
    matches.sort_by(|a, b| a.id.cmp(&b.id));
    matches.dedup_by(|a, b| a.id == b.id);
    match matches.as_slice() {
        [] => None,
        [only] => Some(((*only).clone(), WorkReason::Matcher)),
        many => {
            conflicts.push(Conflict::AmbiguousWork {
                work_ids: many.iter().map(|w| w.id.clone()).collect(),
            });
            None
        }
    }
}

/// Credits of `listing` at `level`, as writes on `target_id`.
fn credit_writes(listing: &Listing, level: Level, target_id: &str) -> Vec<CreditWrite> {
    listing
        .credits_at(level)
        .zip(0u32..)
        .map(|(credit, position)| CreditWrite {
            level,
            target_id: target_id.to_string(),
            credit: credit.clone(),
            position,
        })
        .collect()
}

/// Resolves one listing against a snapshot of the stored rows.
///
/// # Arguments
///
/// * `listing` - The validated listing.
/// * `holding` - Who holds it and how.
/// * `snapshot` - The stored rows that bear on it ([`Snapshot`] states
///   what the caller must include).
/// * `ids` - Mints ids for the rows the plan creates.
///
/// # Returns
///
/// The plan of writes. Resolving the same listing again after applying
/// the plan returns a plan for which [`Plan::is_noop`] holds.
pub fn resolve(
    listing: &Listing,
    holding: &HoldingDraft,
    snapshot: &Snapshot,
    ids: &mut dyn IdSource,
) -> Plan {
    let mut conflicts = Vec::new();

    let pinned = snapshot.overrides.iter().find_map(|o| match o {
        Override::PinListing {
            listing: key,
            edition_id,
        } if *key == listing.key && snapshot.edition(edition_id).is_some() => {
            Some(edition_id.clone())
        }
        Override::PinListing { .. } | Override::IsolateWork { .. } => None,
    });
    let stored = snapshot
        .listing
        .as_ref()
        .filter(|row| row.key == listing.key && snapshot.edition(&row.edition_id).is_some());
    let mut strong_owners = Vec::new();
    for key in listing
        .edition_identifiers()
        .filter(|key| key.strength() == Strength::Strong)
    {
        push_unique(
            &mut strong_owners,
            owners(snapshot, key).into_iter().map(str::to_string),
        );
    }

    let found = if let Some(edition_id) = pinned {
        Some((edition_id, LinkReason::Pinned))
    } else if let Some(row) = stored {
        Some((row.edition_id.clone(), LinkReason::Listing))
    } else if let Some(first) = strong_owners.first() {
        Some((first.clone(), LinkReason::StrongIdentifier))
    } else {
        weak_candidate(snapshot, listing).map(|id| (id, LinkReason::WeakIdentifier))
    };

    let mut new_work = None;
    let mut new_expression = None;
    let mut new_edition = None;
    let mut work_reason = None;
    let (edition_id, link, work_id) = match found {
        Some((edition_id, link)) => {
            let work_id = snapshot
                .work_of_edition(&edition_id)
                .map(|w| w.id.clone())
                .unwrap_or_default();
            (edition_id, link, work_id)
        }
        None => {
            let parent_work_id = match listing.work_kind {
                WorkKind::PodcastEpisode => snapshot
                    .parent_listing
                    .as_ref()
                    .filter(|row| Some(&row.key) == listing.parent_key().as_ref())
                    .and_then(|row| snapshot.work_of_edition(&row.edition_id))
                    .map(|w| w.id.clone()),
                WorkKind::Book | WorkKind::PodcastShow | WorkKind::Drama => None,
            };
            let (work, reason) =
                match find_work(snapshot, listing, parent_work_id.as_deref(), &mut conflicts) {
                    Some((work, reason)) => (work, reason),
                    None => {
                        let work = WorkRow {
                            id: ids.next_id(EntityKind::Work),
                            kind: listing.work_kind,
                            title: listing.title.clone(),
                            match_key: listing.work_match_key(),
                            parent_work_id,
                        };
                        new_work = Some(work.clone());
                        (work, WorkReason::New)
                    }
                };
            work_reason = Some(reason);
            let expression = (reason != WorkReason::New)
                .then(|| {
                    let mut candidates: Vec<&ExpressionRow> = snapshot
                        .expressions
                        .iter()
                        .filter(|e| e.work_id == work.id && expression_compatible(e, listing))
                        .collect();
                    candidates.sort_by(|a, b| a.id.cmp(&b.id));
                    candidates.first().map(|e| e.id.clone())
                })
                .flatten();
            let expression_id = match expression {
                Some(id) => id,
                None => {
                    let row = ExpressionRow {
                        id: ids.next_id(EntityKind::Expression),
                        work_id: work.id.clone(),
                        kind: listing.expression_kind,
                        language: listing.language.clone(),
                        abridgement: listing.abridgement,
                        credit_key: listing.expression_credit_key(),
                    };
                    let id = row.id.clone();
                    new_expression = Some(row);
                    id
                }
            };
            let edition = EditionRow {
                id: ids.next_id(EntityKind::Edition),
                expression_id,
                media_kind: listing.media_kind,
                title: listing.title.clone(),
                subtitle: listing.subtitle.clone(),
                published_at: listing.published_at.clone(),
                duration_minutes: listing.duration_minutes,
            };
            let edition_id = edition.id.clone();
            new_edition = Some(edition);
            (edition_id, LinkReason::New, work.id)
        }
    };

    if strong_owners.iter().any(|owner| *owner != edition_id) {
        let mut edition_ids = vec![edition_id.clone()];
        push_unique(&mut edition_ids, strong_owners.iter().cloned());
        conflicts.push(Conflict::StrongIdentifiers { edition_ids });
    }

    let mut edition_identifiers = Vec::new();
    for key in listing.edition_identifiers() {
        let held = snapshot
            .edition_identifiers
            .iter()
            .any(|row| row.edition_id == edition_id && &row.key == key);
        if held {
            continue;
        }
        if key.strength() == Strength::Strong {
            // Owned elsewhere (a conflict recorded above), or a second value
            // of a scheme the edition already has: never attached.
            if !owners(snapshot, key).is_empty() {
                continue;
            }
            if clashes(snapshot, &edition_id, key) {
                let conflict = Conflict::StrongIdentifiers {
                    edition_ids: vec![edition_id.clone()],
                };
                if !conflicts.contains(&conflict) {
                    conflicts.push(conflict);
                }
                continue;
            }
            // Two strong values of one scheme on one listing: keep the first.
            if edition_identifiers
                .iter()
                .any(|row: &EditionIdentifierRow| {
                    row.key.scheme == key.scheme && row.key.marketplace == key.marketplace
                })
            {
                continue;
            }
        }
        edition_identifiers.push(EditionIdentifierRow {
            edition_id: edition_id.clone(),
            key: key.clone(),
        });
    }

    let mut work_identifiers = Vec::new();
    if !work_id.is_empty() {
        for key in listing.work_identifiers() {
            let holders = work_owners(snapshot, key);
            if holders.contains(&work_id.as_str()) {
                continue;
            }
            if holders.is_empty() {
                work_identifiers.push(WorkIdentifierRow {
                    work_id: work_id.clone(),
                    key: key.clone(),
                });
                continue;
            }
            let mut work_ids = vec![work_id.clone()];
            push_unique(&mut work_ids, holders.into_iter().map(str::to_string));
            let conflict = Conflict::WorkIdentifiers { work_ids };
            if !conflicts.contains(&conflict) {
                conflicts.push(conflict);
            }
        }
    }

    // Credits and series go on every level that has none yet, so a new
    // entity gets the listing's, and one whose credits a failed batch never
    // wrote gets them on the next scan.
    let expression_id = new_expression.as_ref().map_or_else(
        || {
            snapshot
                .edition(&edition_id)
                .map(|e| e.expression_id.clone())
                .unwrap_or_default()
        },
        |e| e.id.clone(),
    );
    let uncredited = |level: Level, id: &str| {
        !id.is_empty()
            && !snapshot
                .credited
                .iter()
                .any(|(l, target)| *l == level && target == id)
    };
    let mut credits = Vec::new();
    for (level, target) in [
        (Level::Work, work_id.as_str()),
        (Level::Expression, expression_id.as_str()),
        (Level::Edition, edition_id.as_str()),
    ] {
        if uncredited(level, target) {
            credits.extend(credit_writes(listing, level, target));
        }
    }
    let mut series = Vec::new();
    if !work_id.is_empty() && !snapshot.works_with_series.contains(&work_id) {
        series.extend(listing.series.iter().map(|entry| SeriesWrite {
            work_id: work_id.clone(),
            entry: entry.clone(),
        }));
    }

    // A listing joining a stored edition only fills what the edition
    // lacks: two listings of one edition that disagree must not overwrite
    // each other on every scan.
    let edition_update = match link {
        LinkReason::Listing
        | LinkReason::Pinned
        | LinkReason::StrongIdentifier
        | LinkReason::WeakIdentifier => snapshot.edition(&edition_id).and_then(|current| {
            let updated = EditionRow {
                subtitle: current
                    .subtitle
                    .clone()
                    .or_else(|| listing.subtitle.clone()),
                published_at: current
                    .published_at
                    .clone()
                    .or_else(|| listing.published_at.clone()),
                duration_minutes: current.duration_minutes.or(listing.duration_minutes),
                ..current.clone()
            };
            (updated != *current).then_some(updated)
        }),
        LinkReason::New => None,
    };

    let (listing_row, listing_is_new, relinked) = match stored {
        Some(row) => (
            ListingRow {
                edition_id: edition_id.clone(),
                ..row.clone()
            },
            false,
            row.edition_id != edition_id,
        ),
        None => (
            ListingRow {
                id: snapshot
                    .listing
                    .as_ref()
                    .filter(|row| row.key == listing.key)
                    .map_or_else(|| ids.next_id(EntityKind::Listing), |row| row.id.clone()),
                key: listing.key.clone(),
                edition_id: edition_id.clone(),
            },
            snapshot
                .listing
                .as_ref()
                .is_none_or(|row| row.key != listing.key),
            true,
        ),
    };

    let stored_holding = snapshot
        .holding
        .as_ref()
        .filter(|h| h.listing_id == listing_row.id && h.holder == holding.holder);
    let holding_row = HoldingRow {
        listing_id: listing_row.id.clone(),
        holder: holding.holder.clone(),
        entitlement: holding.entitlement,
        purchased_at: holding
            .purchased_at
            .clone()
            .or_else(|| stored_holding.and_then(|h| h.purchased_at.clone())),
    };
    let holding_is_new = stored_holding.is_none();
    let holding_changed = stored_holding.is_some_and(|stored| *stored != holding_row);

    // An episode first seen before its show is stored without one; link it
    // once the show's listing has been resolved.
    let work_parent = if listing.work_kind == WorkKind::PodcastEpisode && new_work.is_none() {
        let show = snapshot
            .parent_listing
            .as_ref()
            .filter(|row| Some(&row.key) == listing.parent_key().as_ref())
            .and_then(|row| snapshot.work_of_edition(&row.edition_id))
            .filter(|w| w.kind == WorkKind::PodcastShow);
        snapshot
            .work(&work_id)
            .filter(|w| w.kind == WorkKind::PodcastEpisode && w.parent_work_id.is_none())
            .zip(show)
            .map(|(episode, show)| WorkParent {
                work_id: episode.id.clone(),
                parent_work_id: show.id.clone(),
            })
    } else {
        None
    };

    let mut events = Vec::new();
    if listing_is_new || relinked {
        events.push(Event::EditionLinked {
            edition_id: edition_id.clone(),
            listing_id: listing_row.id.clone(),
        });
    }
    if edition_update.is_some() {
        events.push(Event::EditionUpdated {
            edition_id: edition_id.clone(),
        });
    }
    if holding_is_new {
        events.push(Event::HoldingAdded {
            listing_id: listing_row.id.clone(),
            holder: holding.holder.clone(),
        });
    }

    Plan {
        listing: listing_row,
        listing_is_new,
        link,
        work_reason,
        new_work,
        new_expression,
        new_edition,
        edition_update,
        work_parent,
        edition_identifiers,
        work_identifiers,
        credits,
        series,
        holding: holding_row,
        holding_is_new,
        holding_changed,
        conflicts,
        events,
    }
}
