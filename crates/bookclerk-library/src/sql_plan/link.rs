//! Compiles a resolved listing's writes into one guarded SQL batch.
//!
//! Every write carries its own guard in a `WHERE` clause so no statement
//! depends on reading an earlier one's result in the host:
//!
//! - the new work, expression and edition are inserted only while the
//!   listing is still unstored and none of its strong identifiers has an
//!   owner (another writer that got there first makes them no-ops);
//! - identifiers, the listing and the holding are written only when the
//!   edition they reference exists;
//! - a stored listing is refreshed only while it still sells the edition
//!   the plan was made from;
//! - identifiers and the listing never join an edition when another
//!   writer gave it a different value of one of the plan's strong schemes,
//!   or gave one of the plan's strong identifiers to another edition;
//! - conflicts and events are written only when the listing now sells the
//!   planned edition, which is also the batch's `ok` / `retry` outcome.
//!   Events also ignore a dedup-key clash, which a concurrent batch for the
//!   same holding can cause between its `NOT EXISTS` check and insert.

use bookclerk_plugin_abi::{DbType, DbValue};

use super::named::{j_i64, j_opt_i64, j_opt_str, j_str, sql, SqlStmt};
use crate::atomic_ops::atomic_status;
use crate::library_link::{CreditBatchParams, LinkWorkRow, ListingLinkParams};

/// Real bind, or a typed REAL null.
fn j_opt_f64(n: Option<f64>) -> DbValue {
    match n {
        Some(v) if v.is_finite() => DbValue::Float64(v),
        Some(_) | None => DbValue::Null(DbType::Float64),
    }
}

/// `n` comma-separated placeholders.
fn placeholders(n: usize) -> String {
    vec!["?"; n].join(", ")
}

/// The guard on a new work, expression or edition, with its binds:
/// the listing is unstored (when the plan inserts it) and no strong key
/// has an owner.
fn creation_gate(p: &ListingLinkParams) -> (String, Vec<DbValue>) {
    let mut parts = vec!["1 = 1".to_string()];
    let mut binds = Vec::new();
    if p.listing_is_new {
        parts.push(
            "NOT EXISTS (SELECT 1 FROM store_listings \
             WHERE plugin_key = ? AND marketplace = ? AND product_id = ?)"
                .to_string(),
        );
        binds.extend([
            j_str(&p.plugin_key),
            j_str(&p.marketplace),
            j_str(&p.product_id),
        ]);
    }
    if !p.gate_strong_keys.is_empty() {
        parts.push(format!(
            "NOT EXISTS (SELECT 1 FROM edition_identifiers WHERE strong_key IN ({}))",
            placeholders(p.gate_strong_keys.len())
        ));
        binds.extend(p.gate_strong_keys.iter().map(|k| j_str(k)));
    }
    // The plan only attaches work identifiers no work owned when it was
    // made; one claimed since means the new rows would split the work.
    for id in &p.work_identifiers {
        parts.push(
            "NOT EXISTS (SELECT 1 FROM work_identifiers WHERE scheme = ? AND value = ?)"
                .to_string(),
        );
        binds.extend([j_str(&id.scheme), j_str(&id.value)]);
    }
    (parts.join(" AND "), binds)
}

/// The predicate "since the snapshot, the edition gained another value of
/// one of the plan's strong schemes, another edition claimed one of the
/// plan's strong identifiers, or another work claimed one of the plan's
/// work identifiers", with its binds.
///
/// Any of these changes by another writer between the snapshot and this
/// batch means the plan's identifiers and listing must not be attached;
/// the batch answers `retry` so the resolver sees the new state.
fn strong_clash(p: &ListingLinkParams) -> (String, Vec<DbValue>) {
    let mut parts = Vec::new();
    let mut binds = Vec::new();
    for id in &p.work_identifiers {
        parts.push(
            "EXISTS (SELECT 1 FROM work_identifiers WHERE scheme = ? AND value = ? \
             AND work_id <> ?)",
        );
        binds.extend([j_str(&id.scheme), j_str(&id.value), j_str(&p.work_id)]);
    }
    for id in p
        .edition_identifiers
        .iter()
        .filter(|id| id.strong_key.is_some())
    {
        parts.push(
            "EXISTS (SELECT 1 FROM edition_identifiers WHERE edition_id = ? AND scheme = ? \
             AND marketplace = ? AND strong_key IS NOT NULL AND value <> ?)",
        );
        binds.extend([
            j_str(&p.edition_id),
            j_str(&id.scheme),
            j_str(&id.marketplace),
            j_str(&id.value),
        ]);
        // The plan only attaches strong identifiers no edition owned when
        // it was made; another edition claiming one since is a clash too.
        parts.push(
            "EXISTS (SELECT 1 FROM edition_identifiers WHERE strong_key = ? AND edition_id <> ?)",
        );
        binds.extend([j_opt_str(id.strong_key.as_deref()), j_str(&p.edition_id)]);
    }
    if parts.is_empty() {
        return ("1 = 0".to_string(), Vec::new());
    }
    (format!("({})", parts.join(" OR ")), binds)
}

/// The predicate "nobody stored or relinked the listing since the
/// snapshot", with its binds: it is still unstored when the plan inserts
/// it, or still sells the edition the plan was made from.
///
/// Identifiers are attached before the listing is written, so they carry
/// this guard themselves; otherwise a batch that loses the listing to
/// another writer would still leave its identifiers on an edition the
/// listing does not sell.
fn listing_unchanged(p: &ListingLinkParams) -> (String, Vec<DbValue>) {
    if p.listing_is_new {
        return (
            "NOT EXISTS (SELECT 1 FROM store_listings \
             WHERE plugin_key = ? AND marketplace = ? AND product_id = ?)"
                .to_string(),
            vec![
                j_str(&p.plugin_key),
                j_str(&p.marketplace),
                j_str(&p.product_id),
            ],
        );
    }
    match &p.stored_edition_id {
        Some(stored) => (
            "EXISTS (SELECT 1 FROM store_listings WHERE id = ? AND edition_id = ?)".to_string(),
            vec![j_str(&p.listing_id), j_str(stored)],
        ),
        None => ("1 = 1".to_string(), Vec::new()),
    }
}

/// The predicate "no work the matcher would have chosen appeared since the
/// snapshot", with its binds, for the new work `w`.
///
/// The plan creates a work only when the matcher found none (or several)
/// with its key; a same-key work another writer stored since would make
/// two works of one book, so the work is held back and the batch answers
/// `retry`. Works the snapshot already held stay allowed.
fn new_rivals(p: &ListingLinkParams, w: &LinkWorkRow) -> (String, Vec<DbValue>) {
    if w.match_key.is_empty() {
        return ("1 = 1".to_string(), Vec::new());
    }
    let mut sql =
        "NOT EXISTS (SELECT 1 FROM works o WHERE o.match_key = ? AND o.kind = ?".to_string();
    let mut binds = vec![j_str(&w.match_key), j_str(&w.kind)];
    match (w.kind.as_str(), &w.parent_work_id) {
        // The matcher only joins episodes within one known show.
        ("podcast_episode", None) => return ("1 = 1".to_string(), Vec::new()),
        ("podcast_episode", Some(parent)) => {
            sql.push_str(" AND o.parent_work_id = ?");
            binds.push(j_str(parent));
        }
        _ => {}
    }
    if !p.known_rivals.is_empty() {
        sql.push_str(&format!(
            " AND o.id NOT IN ({})",
            placeholders(p.known_rivals.len())
        ));
        binds.extend(p.known_rivals.iter().map(|id| j_str(id)));
    }
    sql.push(')');
    (sql, binds)
}

/// The predicate "the listing sells the planned edition", with its binds.
fn linked(p: &ListingLinkParams) -> (&'static str, Vec<DbValue>) {
    (
        "EXISTS (SELECT 1 FROM store_listings WHERE id = ? AND edition_id = ?)",
        vec![j_str(&p.listing_id), j_str(&p.edition_id)],
    )
}

/// Compiles `p` into its statements; the last is the outcome `SELECT`.
pub(super) fn plan_link_listing(p: &ListingLinkParams, now: &str) -> Vec<SqlStmt> {
    let mut out = Vec::new();
    let (gate, gate_binds) = creation_gate(p);
    let (is_linked, linked_binds) = linked(p);
    let (clash, clash_binds) = strong_clash(p);
    let (unchanged, unchanged_binds) = listing_unchanged(p);

    if let Some(w) = &p.new_work {
        let mut binds = vec![
            j_str(&w.id),
            j_str(&w.kind),
            j_str(&w.title),
            j_str(&w.match_key),
            j_opt_str(w.parent_work_id.as_deref()),
            j_str(now),
            j_str(now),
        ];
        binds.extend(gate_binds.clone());
        let (rivals, rival_binds) = new_rivals(p, w);
        binds.extend(rival_binds);
        out.push(sql(
            &format!(
                "INSERT INTO works (id, kind, title, match_key, parent_work_id, created_at, updated_at) \
                 SELECT ?, ?, ?, ?, (SELECT p.id FROM works p WHERE p.id = ?), ?, ? \
                 WHERE {gate} AND {rivals}"
            ),
            binds,
        ));
    }
    if let Some(x) = &p.new_expression {
        let mut binds = vec![
            j_str(&x.id),
            j_str(&x.work_id),
            j_str(&x.kind),
            j_opt_str(x.language.as_deref()),
            j_str(&x.abridgement),
            j_str(&x.credit_key),
            j_str(now),
            j_str(now),
            j_str(&x.work_id),
        ];
        binds.extend(gate_binds.clone());
        out.push(sql(
            &format!(
                "INSERT INTO expressions (id, work_id, kind, language, abridgement, credit_key, created_at, updated_at) \
                 SELECT ?, ?, ?, ?, ?, ?, ?, ? \
                 WHERE EXISTS (SELECT 1 FROM works WHERE id = ?) AND {gate}"
            ),
            binds,
        ));
    }
    if let Some(e) = &p.new_edition {
        let mut binds = vec![
            j_str(&e.id),
            j_str(&e.expression_id),
            j_str(&e.media_kind),
            j_str(&e.title),
            j_opt_str(e.subtitle.as_deref()),
            j_opt_str(e.published_at.as_deref()),
            j_opt_i64(e.duration_minutes),
            j_str(now),
            j_str(now),
            j_str(&e.expression_id),
        ];
        binds.extend(gate_binds);
        out.push(sql(
            &format!(
                "INSERT INTO editions (id, expression_id, media_kind, title, subtitle, published_at, duration_minutes, created_at, updated_at) \
                 SELECT ?, ?, ?, ?, ?, ?, ?, ?, ? \
                 WHERE EXISTS (SELECT 1 FROM expressions WHERE id = ?) AND {gate}"
            ),
            binds,
        ));
    }
    // Updates to stored rows carry the listing guard too: a plan that lost
    // the listing to another writer must not fill rows it no longer sells.
    if let Some(fill) = &p.edition_fill {
        let mut binds = vec![
            j_opt_str(fill.subtitle.as_deref()),
            j_opt_str(fill.published_at.as_deref()),
            j_opt_i64(fill.duration_minutes),
            j_str(now),
            j_str(&p.edition_id),
        ];
        binds.extend(unchanged_binds.clone());
        out.push(sql(
            &format!(
                "UPDATE editions SET subtitle = COALESCE(subtitle, ?), \
                 published_at = COALESCE(published_at, ?), \
                 duration_minutes = COALESCE(duration_minutes, ?), updated_at = ? \
                 WHERE id = ? AND {unchanged}"
            ),
            binds,
        ));
    }
    if let Some(fill) = &p.expression_fill {
        let mut binds = vec![
            j_opt_str(fill.language.as_deref()),
            j_str(&fill.abridgement),
            j_str(&fill.abridgement),
            j_str(&fill.credit_key),
            j_str(&fill.credit_key),
            j_str(now),
            j_str(&fill.expression_id),
            j_str(&fill.kind),
        ];
        binds.extend(unchanged_binds.clone());
        out.push(sql(
            &format!(
                "UPDATE expressions SET language = COALESCE(language, ?), \
                 abridgement = CASE WHEN abridgement = 'unknown' AND ? <> 'unknown' THEN ? ELSE abridgement END, \
                 credit_key = CASE WHEN credit_key = '' AND ? <> '' THEN ? ELSE credit_key END, \
                 updated_at = ? \
                 WHERE id = ? AND kind = ? AND {unchanged}"
            ),
            binds,
        ));
    }
    if let Some(link) = &p.work_parent {
        let mut binds = vec![
            j_str(&link.parent_work_id),
            j_str(now),
            j_str(&link.work_id),
            j_str(&link.parent_work_id),
        ];
        binds.extend(unchanged_binds.clone());
        out.push(sql(
            &format!(
                "UPDATE works SET parent_work_id = ?, updated_at = ? \
                 WHERE id = ? AND kind = 'podcast_episode' AND parent_work_id IS NULL \
                   AND EXISTS (SELECT 1 FROM works p WHERE p.id = ? AND p.kind = 'podcast_show') \
                   AND {unchanged}"
            ),
            binds,
        ));
    }
    if let Some(fill) = &p.work_match_key {
        let mut binds = vec![
            j_str(&fill.match_key),
            j_str(now),
            j_str(&fill.work_id),
            j_str(&fill.match_key),
            j_str(&fill.kind),
            j_str(&fill.work_id),
        ];
        // Episodes only compete for a key within their show.
        let same_show = match (&fill.parent_work_id, fill.kind.as_str()) {
            (Some(parent), _) => {
                binds.push(j_str(parent));
                " AND o.parent_work_id = ?"
            }
            (None, "podcast_episode") => " AND o.parent_work_id IS NULL",
            (None, _) => "",
        };
        binds.extend(unchanged_binds.clone());
        out.push(sql(
            &format!(
                "UPDATE works SET match_key = ?, updated_at = ? \
                 WHERE id = ? AND match_key = '' \
                   AND NOT EXISTS (SELECT 1 FROM works o WHERE o.match_key = ? \
                   AND o.kind = ? AND o.id <> ?{same_show}) AND {unchanged}"
            ),
            binds,
        ));
    }
    for id in &p.edition_identifiers {
        let mut binds = vec![
            j_str(&p.edition_id),
            j_str(&id.scheme),
            j_str(&id.value),
            j_str(&id.marketplace),
            j_str(&id.strength),
            j_opt_str(id.strong_key.as_deref()),
            j_str(now),
            j_str(&p.edition_id),
        ];
        binds.extend(clash_binds.clone());
        binds.extend(unchanged_binds.clone());
        out.push(sql(
            &format!(
                "INSERT OR IGNORE INTO edition_identifiers \
                 (edition_id, scheme, value, marketplace, strength, strong_key, created_at) \
                 SELECT ?, ?, ?, ?, ?, ?, ? \
                 WHERE EXISTS (SELECT 1 FROM editions WHERE id = ?) AND NOT {clash} \
                 AND {unchanged}"
            ),
            binds,
        ));
    }
    for id in &p.work_identifiers {
        let mut binds = vec![
            j_str(&id.scheme),
            j_str(&id.value),
            j_str(&p.work_id),
            j_str(now),
            j_str(&p.work_id),
        ];
        binds.extend(clash_binds.clone());
        binds.extend(unchanged_binds.clone());
        out.push(sql(
            &format!(
                "INSERT OR IGNORE INTO work_identifiers (scheme, value, work_id, created_at) \
                 SELECT ?, ?, ?, ? WHERE EXISTS (SELECT 1 FROM works WHERE id = ?) \
                 AND NOT {clash} AND {unchanged}"
            ),
            binds,
        ));
    }
    if p.listing_is_new {
        let mut binds = vec![
            j_str(&p.listing_id),
            j_str(&p.plugin_key),
            j_str(&p.marketplace),
            j_str(&p.product_id),
            j_str(&p.edition_id),
            j_str(&p.title),
            j_opt_str(p.categories.as_deref()),
            j_str(now),
            j_str(now),
            j_str(now),
            j_str(&p.edition_id),
        ];
        binds.extend(clash_binds.clone());
        out.push(sql(
            &format!(
                "INSERT OR IGNORE INTO store_listings \
                 (id, plugin_key, marketplace, product_id, edition_id, title, categories, observed_at, created_at, updated_at) \
                 SELECT ?, ?, ?, ?, ?, ?, ?, ?, ?, ? \
                 WHERE EXISTS (SELECT 1 FROM editions WHERE id = ?) AND NOT {clash}"
            ),
            binds,
        ));
    } else {
        let mut binds = vec![
            j_str(&p.edition_id),
            j_str(&p.title),
            j_opt_str(p.categories.as_deref()),
            j_str(now),
            j_str(now),
            j_str(&p.listing_id),
            j_str(&p.edition_id),
        ];
        // Only refresh the listing while it still sells the edition the
        // plan was made from; a concurrent relink turns this into a no-op
        // and the outcome into `retry`.
        let still_stored = match &p.stored_edition_id {
            Some(stored) => {
                binds.push(j_str(stored));
                " AND edition_id = ?"
            }
            None => "",
        };
        binds.extend(clash_binds);
        out.push(sql(
            &format!(
                "UPDATE store_listings SET edition_id = ?, title = ?, \
                 categories = COALESCE(?, categories), observed_at = ?, updated_at = ? \
                 WHERE id = ? AND EXISTS (SELECT 1 FROM editions WHERE id = ?){still_stored} \
                 AND NOT {clash}"
            ),
            binds,
        ));
    }
    let mut binds = vec![
        j_str(&p.listing_id),
        j_str(&p.holder),
        j_str(&p.entitlement),
        j_opt_str(p.purchased_at.as_deref()),
        j_str(now),
        j_str(now),
    ];
    binds.extend(linked_binds.clone());
    out.push(sql(
        &format!(
            "INSERT OR IGNORE INTO holdings \
             (listing_id, holder, entitlement, purchased_at, created_at, updated_at) \
             SELECT ?, ?, ?, ?, ?, ? WHERE {is_linked}"
        ),
        binds,
    ));
    out.push(sql(
        "UPDATE holdings SET entitlement = ?, purchased_at = COALESCE(?, purchased_at), \
         updated_at = ? WHERE listing_id = ? AND holder = ?",
        vec![
            j_str(&p.entitlement),
            j_opt_str(p.purchased_at.as_deref()),
            j_str(now),
            j_str(&p.listing_id),
            j_str(&p.holder),
        ],
    ));
    for conflict in &p.conflicts {
        let mut binds = vec![
            j_str(&p.listing_id),
            j_str(&conflict.kind),
            j_str(&conflict.subject_ids),
            j_str(now),
            j_str(now),
        ];
        binds.extend(linked_binds.clone());
        out.push(sql(
            &format!(
                "INSERT OR IGNORE INTO resolution_conflicts \
                 (listing_id, kind, subject_ids, created_at, updated_at) \
                 SELECT ?, ?, ?, ?, ? WHERE {is_linked}"
            ),
            binds,
        ));
    }
    for event in &p.events {
        let mut binds = vec![
            j_str(&event.id),
            j_str(&event.event_type),
            j_i64(1),
            j_str(now),
            j_str(&event.account_id),
            j_str(&event.source),
            j_str(&p.listing_id),
            j_str(""),
            j_str(&event.dedup_key),
            j_str(&event.payload),
            j_str(&event.ordering_key),
            j_str(now),
            j_str(&event.account_id),
            j_str(&event.source),
            j_str(&event.event_type),
            j_str(&event.dedup_key),
        ];
        binds.extend(linked_binds.clone());
        out.push(sql(
            &format!(
                "INSERT OR IGNORE INTO domain_events (\
                    id, event_type, schema_version, occurred_at, account_id, source, \
                    correlation_id, causation_id, dedup_key, payload, ordering_key, \
                    dispatch_state, created_at, wake_pending\
                 ) SELECT ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'pending', ?, 1 \
                 WHERE NOT EXISTS (\
                    SELECT 1 FROM domain_events \
                    WHERE queue_name = 'core.events' AND account_id = ? AND source = ? \
                      AND event_type = ? AND dedup_key = ?\
                 ) AND {is_linked}"
            ),
            binds,
        ));
    }
    out.push(sql(
        &format!(
            "SELECT CASE WHEN {is_linked} THEN '{ok}' ELSE '{retry}' END AS status",
            ok = atomic_status::OK,
            retry = atomic_status::RETRY,
        ),
        linked_binds,
    ));
    out
}

/// Compiles `p` into idempotent inserts; the last is the outcome `SELECT`.
pub(super) fn plan_write_credits(p: &CreditBatchParams, now: &str) -> Vec<SqlStmt> {
    let mut out = Vec::new();
    for c in &p.contributors {
        out.push(sql(
            "INSERT OR IGNORE INTO contributors (id, name, sort_name, name_key, created_at) \
             SELECT ?, ?, ?, ?, ?",
            vec![
                j_str(&c.id),
                j_str(&c.name),
                j_str(&c.sort_name),
                j_str(&c.name_key),
                j_str(now),
            ],
        ));
    }
    for c in &p.credits {
        out.push(sql(
            "INSERT OR IGNORE INTO credits \
             (target_kind, target_id, contributor_id, role, position, created_at) \
             SELECT ?, ?, ?, ?, ?, ? WHERE EXISTS (SELECT 1 FROM contributors WHERE id = ?)",
            vec![
                j_str(&c.target_kind),
                j_str(&c.target_id),
                j_str(&c.contributor_id),
                j_str(&c.role),
                j_i64(c.position),
                j_str(now),
                j_str(&c.contributor_id),
            ],
        ));
    }
    for s in &p.series {
        out.push(sql(
            "INSERT OR IGNORE INTO series (id, name, name_key, created_at) SELECT ?, ?, ?, ?",
            vec![j_str(&s.id), j_str(&s.name), j_str(&s.name_key), j_str(now)],
        ));
    }
    for e in &p.series_entries {
        out.push(sql(
            "INSERT OR IGNORE INTO series_entries \
             (series_id, work_id, position, sort_position, created_at) \
             SELECT ?, ?, ?, ?, ? \
             WHERE EXISTS (SELECT 1 FROM series WHERE id = ?) \
               AND EXISTS (SELECT 1 FROM works WHERE id = ?)",
            vec![
                j_str(&e.series_id),
                j_str(&e.work_id),
                j_opt_str(e.position.as_deref()),
                j_opt_f64(e.sort_position),
                j_str(now),
                j_str(&e.series_id),
                j_str(&e.work_id),
            ],
        ));
    }
    out.push(sql(
        &format!("SELECT '{ok}' AS status", ok = atomic_status::OK),
        Vec::new(),
    ));
    out
}
