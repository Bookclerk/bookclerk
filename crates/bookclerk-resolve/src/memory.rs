//! An in-memory library that reads snapshots and applies plans.
//!
//! It keeps the same rows and the same constraints as the SQL tables
//! (unique strong identifiers, unique listings, rows that reference rows
//! which exist), so tests and the fuzz oracle can resolve sequences of
//! listings without a database. The store's SQL reads follow
//! [`MemoryCatalog::snapshot`] and its batch writes follow
//! [`MemoryCatalog::apply`].

use std::collections::{BTreeMap, BTreeSet};

use crate::identifiers::IdentifierKey;
use crate::kinds::Level;
use crate::listing::{Credit, Holder, HoldingDraft, Listing, ListingKey, SeriesEntry};
use crate::model::{
    Conflict, EditionIdentifierRow, EditionRow, ExpressionRow, HoldingRow, LinkReason, ListingRow,
    Override, Plan, Snapshot, WorkIdentifierRow, WorkReason, WorkRow,
};
use crate::resolve::{resolve, EntityKind, IdSource};

/// Mints sequential ids (`work-1`, `edition-1`, …) for tests and fuzzing.
#[derive(Debug, Default, Clone)]
pub struct CounterIds {
    /// The last number handed out.
    next: u64,
}

impl IdSource for CounterIds {
    fn next_id(&mut self, kind: EntityKind) -> String {
        self.next += 1;
        let prefix = match kind {
            EntityKind::Work => "work",
            EntityKind::Expression => "expression",
            EntityKind::Edition => "edition",
            EntityKind::Listing => "listing",
        };
        format!("{prefix}-{}", self.next)
    }
}

/// A plan broke a constraint the SQL schema enforces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyError(pub String);

impl std::fmt::Display for ApplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "plan breaks a constraint: {}", self.0)
    }
}

impl std::error::Error for ApplyError {}

/// What linked one listing, kept for the oracle.
#[derive(Debug, Clone, PartialEq)]
pub struct LinkRecord {
    /// The listing as resolved.
    pub listing: Listing,
    /// Who held it.
    pub holding: HoldingDraft,
    /// The edition it reached.
    pub edition_id: String,
    /// Why.
    pub link: LinkReason,
    /// How a new edition found its work.
    pub work_reason: Option<WorkReason>,
    /// The parent show's work when the listing was linked, for an episode.
    pub parent_work_id: Option<String>,
}

/// The in-memory library.
#[derive(Debug, Default, Clone)]
pub struct MemoryCatalog {
    /// Works by id.
    pub works: BTreeMap<String, WorkRow>,
    /// Expressions by id.
    pub expressions: BTreeMap<String, ExpressionRow>,
    /// Editions by id.
    pub editions: BTreeMap<String, EditionRow>,
    /// Edition identifiers.
    pub edition_identifiers: BTreeSet<EditionIdentifierRow>,
    /// Work identifiers by key (unique).
    pub work_identifiers: BTreeMap<IdentifierKey, String>,
    /// Listings by key.
    pub listings: BTreeMap<ListingKey, ListingRow>,
    /// Holdings by listing id and holder.
    pub holdings: BTreeMap<(String, Holder), HoldingRow>,
    /// Credits written: level, target id, credit and billing position.
    pub credits: Vec<(Level, String, Credit, u32)>,
    /// Series entries: work id and entry.
    pub series: Vec<(String, SeriesEntry)>,
    /// Overrides.
    pub overrides: Vec<Override>,
    /// Conflicts recorded, de-duplicated.
    pub conflicts: Vec<Conflict>,
    /// Every link made, in order.
    pub links: Vec<LinkRecord>,
    /// Ids for new rows.
    pub ids: CounterIds,
}

impl MemoryCatalog {
    /// Reads the snapshot [`resolve`] needs for `listing` and `holder`.
    ///
    /// # Arguments
    ///
    /// * `listing` - The listing to resolve.
    /// * `holder` - Who holds it.
    ///
    /// # Returns
    ///
    /// Every stored row that bears on the listing, per [`Snapshot`].
    #[must_use]
    pub fn snapshot(&self, listing: &Listing, holder: &Holder) -> Snapshot {
        let stored = self.listings.get(&listing.key).cloned();
        let parent = listing
            .parent_key()
            .and_then(|key| self.listings.get(&key).cloned());
        let keys: BTreeSet<&IdentifierKey> = listing.edition_identifiers().collect();
        let pins: Vec<Override> = self
            .overrides
            .iter()
            .filter(
                |o| matches!(o, Override::PinListing { listing: key, .. } if *key == listing.key),
            )
            .cloned()
            .collect();

        let mut edition_ids: BTreeSet<String> = BTreeSet::new();
        edition_ids.extend(stored.iter().map(|l| l.edition_id.clone()));
        edition_ids.extend(parent.iter().map(|l| l.edition_id.clone()));
        edition_ids.extend(pins.iter().filter_map(|o| match o {
            Override::PinListing { edition_id, .. } => Some(edition_id.clone()),
            Override::IsolateWork { .. } => None,
        }));
        edition_ids.extend(
            self.edition_identifiers
                .iter()
                .filter(|row| keys.contains(&row.key))
                .map(|row| row.edition_id.clone()),
        );
        edition_ids.retain(|id| self.editions.contains_key(id));

        let edition_identifiers: Vec<EditionIdentifierRow> = self
            .edition_identifiers
            .iter()
            .filter(|row| keys.contains(&row.key) || edition_ids.contains(&row.edition_id))
            .cloned()
            .collect();
        let work_identifiers: Vec<WorkIdentifierRow> = listing
            .work_identifiers()
            .filter_map(|key| {
                self.work_identifiers
                    .get(key)
                    .map(|work_id| WorkIdentifierRow {
                        work_id: work_id.clone(),
                        key: key.clone(),
                    })
            })
            .collect();

        let editions: Vec<EditionRow> = edition_ids
            .iter()
            .filter_map(|id| self.editions.get(id).cloned())
            .collect();
        let mut work_ids: BTreeSet<String> = editions
            .iter()
            .filter_map(|e| self.expressions.get(&e.expression_id))
            .map(|x| x.work_id.clone())
            .collect();
        work_ids.extend(work_identifiers.iter().map(|r| r.work_id.clone()));
        let match_key = listing.work_match_key();
        if !match_key.is_empty() {
            work_ids.extend(
                self.works
                    .values()
                    .filter(|w| w.match_key == match_key && w.kind == listing.work_kind)
                    .map(|w| w.id.clone()),
            );
        }
        let works: Vec<WorkRow> = work_ids
            .iter()
            .filter_map(|id| self.works.get(id).cloned())
            .collect();
        let expressions: Vec<ExpressionRow> = self
            .expressions
            .values()
            .filter(|x| {
                work_ids.contains(&x.work_id) || editions.iter().any(|e| e.expression_id == x.id)
            })
            .cloned()
            .collect();
        let mut overrides = pins;
        overrides.extend(
            self.overrides
                .iter()
                .filter(|o| matches!(o, Override::IsolateWork { work_id } if work_ids.contains(work_id)))
                .cloned(),
        );
        let holding = stored
            .as_ref()
            .and_then(|l| self.holdings.get(&(l.id.clone(), holder.clone())).cloned());
        let credited = self
            .credits
            .iter()
            .filter(|(level, target, _, _)| match level {
                Level::Work => work_ids.contains(target),
                Level::Expression => expressions.iter().any(|x| &x.id == target),
                Level::Edition => edition_ids.contains(target),
            })
            .map(|(level, target, _, _)| (*level, target.clone()))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let works_with_series = self
            .series
            .iter()
            .filter(|(work_id, _)| work_ids.contains(work_id))
            .map(|(work_id, _)| work_id.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        Snapshot {
            listing: stored,
            parent_listing: parent,
            edition_identifiers,
            work_identifiers,
            editions,
            expressions,
            works,
            overrides,
            holding,
            credited,
            works_with_series,
        }
    }

    /// Applies `plan`, checking the constraints the SQL schema enforces.
    ///
    /// # Arguments
    ///
    /// * `plan` - A plan [`resolve`] returned for a snapshot of this catalog.
    ///
    /// # Errors
    ///
    /// Returns [`ApplyError`] when a write would break a constraint; the
    /// catalog may then be partly changed, as a failed batch never is.
    pub fn apply(&mut self, plan: &Plan) -> Result<(), ApplyError> {
        let fail = |what: String| Err(ApplyError(what));
        if let Some(work) = &plan.new_work {
            if self.works.contains_key(&work.id) {
                return fail(format!("work {} exists", work.id));
            }
            if let Some(parent) = &work.parent_work_id {
                if !self.works.contains_key(parent) {
                    return fail(format!("parent work {parent} missing"));
                }
            }
            self.works.insert(work.id.clone(), work.clone());
        }
        if let Some(expression) = &plan.new_expression {
            if !self.works.contains_key(&expression.work_id) {
                return fail(format!("work {} missing", expression.work_id));
            }
            if self
                .expressions
                .insert(expression.id.clone(), expression.clone())
                .is_some()
            {
                return fail(format!("expression {} exists", expression.id));
            }
        }
        if let Some(edition) = &plan.new_edition {
            if !self.expressions.contains_key(&edition.expression_id) {
                return fail(format!("expression {} missing", edition.expression_id));
            }
            if self
                .editions
                .insert(edition.id.clone(), edition.clone())
                .is_some()
            {
                return fail(format!("edition {} exists", edition.id));
            }
        }
        if let Some(update) = &plan.edition_update {
            let Some(current) = self.editions.get_mut(&update.id) else {
                return fail(format!("updated edition {} missing", update.id));
            };
            *current = update.clone();
        }
        for row in &plan.edition_identifiers {
            if !self.editions.contains_key(&row.edition_id) {
                return fail(format!("edition {} missing", row.edition_id));
            }
            if let Some(strong) = row.key.strong_key() {
                let taken = self
                    .edition_identifiers
                    .iter()
                    .any(|r| r.key.strong_key().as_ref() == Some(&strong));
                if taken {
                    return fail(format!("strong identifier {strong:?} taken"));
                }
            }
            if !self.edition_identifiers.insert(row.clone()) {
                return fail(format!("identifier {:?} already on edition", row.key));
            }
        }
        for row in &plan.work_identifiers {
            if !self.works.contains_key(&row.work_id) {
                return fail(format!("work {} missing", row.work_id));
            }
            if self
                .work_identifiers
                .insert(row.key.clone(), row.work_id.clone())
                .is_some()
            {
                return fail(format!("work identifier {:?} taken", row.key));
            }
        }
        for credit in &plan.credits {
            let row = (
                credit.level,
                credit.target_id.clone(),
                credit.credit.clone(),
                credit.position,
            );
            if !self.credits.contains(&row) {
                self.credits.push(row);
            }
        }
        for entry in &plan.series {
            let row = (entry.work_id.clone(), entry.entry.clone());
            if !self.series.contains(&row) {
                self.series.push(row);
            }
        }
        if !self.editions.contains_key(&plan.listing.edition_id) {
            return fail(format!("edition {} missing", plan.listing.edition_id));
        }
        match self.listings.get(&plan.listing.key) {
            Some(row) if plan.listing_is_new || row.id != plan.listing.id => {
                return fail(format!("listing {:?} exists", plan.listing.key));
            }
            None if !plan.listing_is_new => {
                return fail(format!("listing {:?} missing", plan.listing.key));
            }
            Some(_) | None => {}
        }
        self.listings
            .insert(plan.listing.key.clone(), plan.listing.clone());
        let holding_key = (plan.holding.listing_id.clone(), plan.holding.holder.clone());
        if plan.holding_is_new == self.holdings.contains_key(&holding_key) {
            return fail(format!("holding {holding_key:?} newness is wrong"));
        }
        self.holdings.insert(holding_key, plan.holding.clone());
        for conflict in &plan.conflicts {
            if !self.conflicts.contains(conflict) {
                self.conflicts.push(conflict.clone());
            }
        }
        Ok(())
    }

    /// Resolves `listing` for `holding` and applies the plan.
    ///
    /// # Arguments
    ///
    /// * `listing` - The listing.
    /// * `holding` - Who holds it.
    ///
    /// # Returns
    ///
    /// The applied plan.
    ///
    /// # Errors
    ///
    /// Returns [`ApplyError`] when the plan breaks a constraint.
    pub fn record(
        &mut self,
        listing: &Listing,
        holding: &HoldingDraft,
    ) -> Result<Plan, ApplyError> {
        let snapshot = self.snapshot(listing, &holding.holder);
        if let Some(missing) = snapshot.is_closed() {
            return Err(ApplyError(format!("snapshot misses {missing}")));
        }
        let parent_work_id = snapshot
            .parent_listing
            .as_ref()
            .and_then(|row| snapshot.work_of_edition(&row.edition_id))
            .map(|w| w.id.clone());
        let mut ids = self.ids.clone();
        let plan = resolve(listing, holding, &snapshot, &mut ids);
        self.ids = ids;
        self.apply(&plan)?;
        self.links.push(LinkRecord {
            listing: listing.clone(),
            holding: holding.clone(),
            edition_id: plan.listing.edition_id.clone(),
            link: plan.link,
            work_reason: plan.work_reason,
            parent_work_id,
        });
        Ok(plan)
    }

    /// The work an edition realises.
    #[must_use]
    pub fn work_of_edition(&self, edition_id: &str) -> Option<&WorkRow> {
        let edition = self.editions.get(edition_id)?;
        let expression = self.expressions.get(&edition.expression_id)?;
        self.works.get(&expression.work_id)
    }
}
