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
//! - conflicts and events are written only when the listing now sells the
//!   planned edition, which is also the batch's `ok` / `retry` outcome.
//!   Events also ignore a dedup-key clash, which a concurrent batch for the
//!   same holding can cause between its `NOT EXISTS` check and insert.

use bookclerk_plugin_abi::{DbType, DbValue};

use super::named::{j_i64, j_opt_i64, j_opt_str, j_str, sql, SqlStmt};
use crate::atomic_ops::atomic_status;
use crate::library_link::{CreditBatchParams, ListingLinkParams};

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
    (parts.join(" AND "), binds)
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
        out.push(sql(
            &format!(
                "INSERT INTO works (id, kind, title, match_key, parent_work_id, created_at, updated_at) \
                 SELECT ?, ?, ?, ?, (SELECT p.id FROM works p WHERE p.id = ?), ?, ? WHERE {gate}"
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
    if let Some(fill) = &p.edition_fill {
        out.push(sql(
            "UPDATE editions SET subtitle = COALESCE(subtitle, ?), \
             published_at = COALESCE(published_at, ?), \
             duration_minutes = COALESCE(duration_minutes, ?), updated_at = ? \
             WHERE id = ?",
            vec![
                j_opt_str(fill.subtitle.as_deref()),
                j_opt_str(fill.published_at.as_deref()),
                j_opt_i64(fill.duration_minutes),
                j_str(now),
                j_str(&p.edition_id),
            ],
        ));
    }
    for id in &p.edition_identifiers {
        out.push(sql(
            "INSERT OR IGNORE INTO edition_identifiers \
             (edition_id, scheme, value, marketplace, strength, strong_key, created_at) \
             SELECT ?, ?, ?, ?, ?, ?, ? WHERE EXISTS (SELECT 1 FROM editions WHERE id = ?)",
            vec![
                j_str(&p.edition_id),
                j_str(&id.scheme),
                j_str(&id.value),
                j_str(&id.marketplace),
                j_str(&id.strength),
                j_opt_str(id.strong_key.as_deref()),
                j_str(now),
                j_str(&p.edition_id),
            ],
        ));
    }
    for id in &p.work_identifiers {
        out.push(sql(
            "INSERT OR IGNORE INTO work_identifiers (scheme, value, work_id, created_at) \
             SELECT ?, ?, ?, ? WHERE EXISTS (SELECT 1 FROM works WHERE id = ?)",
            vec![
                j_str(&id.scheme),
                j_str(&id.value),
                j_str(&p.work_id),
                j_str(now),
                j_str(&p.work_id),
            ],
        ));
    }
    if p.listing_is_new {
        out.push(sql(
            "INSERT OR IGNORE INTO store_listings \
             (id, plugin_key, marketplace, product_id, edition_id, title, categories, observed_at, created_at, updated_at) \
             SELECT ?, ?, ?, ?, ?, ?, ?, ?, ?, ? WHERE EXISTS (SELECT 1 FROM editions WHERE id = ?)",
            vec![
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
            ],
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
        out.push(sql(
            &format!(
                "UPDATE store_listings SET edition_id = ?, title = ?, \
                 categories = COALESCE(?, categories), observed_at = ?, updated_at = ? \
                 WHERE id = ? AND EXISTS (SELECT 1 FROM editions WHERE id = ?){still_stored}"
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
