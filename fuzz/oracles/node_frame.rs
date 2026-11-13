//! Shared oracle for the `node_frame` fuzz target and the
//! `bookclerk-node-proto` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The input is the text of one node-to-node frame, as a peer node sends it
//! over the cluster socket. The invariants:
//!
//! - parsing never panics, and refuses text over the frame size limit or of
//!   another frame version;
//! - an accepted frame whose serialized text stays within the limit
//!   parses back to an equal frame (a float inside an opaque JSON field may
//!   move by a rounding step, as `serde_json` parses floats without exact
//!   round trips);
//! - every library id an `acquire.targets` frame carries is 1 to 16 ASCII
//!   letters and digits, checked byte by byte here rather than through the
//!   parser's own rule, since a host names storage keys with it;
//! - every marketplace a `catalog.sync` title the library would accept
//!   carries is at most 16 lowercase ASCII letters and digits, checked byte
//!   by byte here, since the library scopes ASIN identifiers to it.
//! - a `catalog.ask` or `catalog.found` frame the library or branch would
//!   accept (`questions_problem` / `found_problem` find nothing) names at
//!   most 100 works, each once, with plain correlation keys; every
//!   identifier is plain ASCII with a marketplace exactly when it is an
//!   ASIN; and every answer lists 1 to 8 distinct editions with a
//!   non-blank title. Each is checked byte by byte here, not through the
//!   validators, since the library writes them into its listing pool;
//! - a `window.page` frame a window would accept (`window_page_problem`
//!   finds nothing for the cursor it asked with) carries at most 100 works,
//!   each once, with plain grouping keys, plain identifiers (a marketplace
//!   exactly on an ASIN), 1 to 8 distinct owned editions with a non-blank
//!   title, and a next cursor past the one asked with, so a window's paging
//!   always moves forward. Checked byte by byte here, since the window
//!   stores them.

use std::collections::BTreeSet;

use bookclerk_node_proto::{
    found_problem, questions_problem, window_page_problem, Availability, Body, Frame, LookupScheme,
    FRAME_VERSION, MAX_FRAME_BYTES, MAX_LIBRARY_ID_BYTES,
};
use serde_json::Value;

/// Runs every invariant on one input; panics on a violation.
pub fn check(data: &[u8]) {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let Ok(frame) = Frame::parse(text) else {
        return;
    };
    assert!(text.len() <= MAX_FRAME_BYTES, "an oversized frame parsed");
    assert_eq!(frame.v, FRAME_VERSION, "another frame version parsed");
    if let Body::AcquireTargets { targets, .. } = &frame.body {
        for target in targets {
            for id in [&target.library_id, &target.parent_library_id]
                .into_iter()
                .flatten()
            {
                let id = id.as_str();
                assert!(
                    !id.is_empty()
                        && id.len() <= MAX_LIBRARY_ID_BYTES
                        && id.bytes().all(|b| b.is_ascii_alphanumeric()),
                    "library id {id:?} accepted"
                );
            }
        }
    }
    if let Body::CatalogSync { titles, .. } = &frame.body {
        for title in titles.iter().filter(|title| title.problem().is_none()) {
            let market = title.marketplace.as_bytes();
            assert!(
                market.len() <= 16
                    && market
                        .iter()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()),
                "marketplace {:?} accepted",
                title.marketplace
            );
        }
    }
    check_lookup(&frame.body);
    check_window(&frame.body);
    let text_again = frame.to_text();
    // Serializing can lengthen an opaque JSON field (`1e9` is written out
    // in full), past the limit the parser enforces: such a frame is never
    // sent again, so there is no round trip to check.
    if text_again.len() > MAX_FRAME_BYTES {
        return;
    }
    let again = Frame::parse(&text_again);
    let Ok(again) = again else {
        panic!("a parsed frame did not parse again: {again:?}");
    };
    assert!(
        same(&json(&again), &json(&frame)),
        "a frame changed across a round trip: {frame:?} became {again:?}"
    );
}

/// True for a plain token: 1 to `max` ASCII letters, digits and `-`.
fn plain(text: &str, max: usize) -> bool {
    !text.is_empty()
        && text.len() <= max
        && text.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// The catalog lookup invariants on an accepted question or answer list.
fn check_lookup(body: &Body) {
    match body {
        Body::CatalogAsk { works } if questions_problem(works).is_none() => {
            assert!(works.len() <= 100, "too many questions accepted");
            let mut seen = BTreeSet::new();
            for work in works {
                assert!(plain(&work.work, 64), "work ref {:?} accepted", work.work);
                assert!(seen.insert(&work.work), "a work asked twice was accepted");
                assert!(
                    (1..=16).contains(&work.identifiers.len()),
                    "a question with {} identifiers accepted",
                    work.identifiers.len()
                );
                for id in &work.identifiers {
                    assert!(plain(&id.value, 64), "identifier {:?} accepted", id.value);
                    let asin = id.scheme == LookupScheme::Asin;
                    assert_eq!(
                        asin,
                        !id.marketplace.is_empty(),
                        "marketplace {:?} accepted on {:?}",
                        id.marketplace,
                        id.scheme
                    );
                }
            }
        }
        Body::CatalogFound { answers, missing } if found_problem(answers, missing).is_none() => {
            assert!(
                answers.len() + missing.len() <= 100,
                "too many answers accepted"
            );
            let mut seen = BTreeSet::new();
            for answer in answers {
                assert!(
                    plain(&answer.work, 64),
                    "work ref {:?} accepted",
                    answer.work
                );
                assert!(
                    seen.insert(&answer.work),
                    "a work answered twice was accepted"
                );
                assert!(
                    (1..=8).contains(&answer.editions.len()),
                    "an answer with {} editions accepted",
                    answer.editions.len()
                );
                let mut keys = BTreeSet::new();
                for edition in &answer.editions {
                    assert!(
                        !edition.metadata.title.trim().is_empty(),
                        "a blank title accepted"
                    );
                    assert!(
                        keys.insert((&edition.store, &edition.marketplace, &edition.product_id)),
                        "an edition listed twice was accepted"
                    );
                }
            }
            for work in missing {
                assert!(plain(work, 64), "missing work ref {work:?} accepted");
                assert!(
                    seen.insert(work),
                    "a work both answered and missing was accepted"
                );
            }
        }
        _ => {}
    }
}

/// The window page invariants on an accepted page, for the cursors a
/// window could have asked with (the first page, and the one just before
/// the page's `next`).
fn check_window(body: &Body) {
    let Body::WindowPage { works, next } = body else {
        return;
    };
    let afters = [0, next.map_or(0, |n| n.saturating_sub(1))];
    for after in afters {
        if window_page_problem(after, works, *next).is_some() {
            continue;
        }
        assert!(works.len() <= 100, "too many window works accepted");
        if let Some(next) = next {
            assert!(
                *next > after,
                "a cursor that does not move forward accepted"
            );
        }
        let mut seen = BTreeSet::new();
        for work in works {
            assert!(plain(&work.work, 64), "work key {:?} accepted", work.work);
            assert!(seen.insert(&work.work), "a work listed twice was accepted");
            assert!(
                work.identifiers.len() <= 16,
                "too many identifiers accepted"
            );
            let mut ids = BTreeSet::new();
            for id in &work.identifiers {
                assert!(plain(&id.value, 64), "identifier {:?} accepted", id.value);
                assert_eq!(
                    id.scheme == LookupScheme::Asin,
                    !id.marketplace.is_empty(),
                    "marketplace {:?} accepted on {:?}",
                    id.marketplace,
                    id.scheme
                );
                assert!(ids.insert(id), "an identifier listed twice was accepted");
            }
            assert!(
                (1..=8).contains(&work.editions.len()),
                "a window work with {} editions accepted",
                work.editions.len()
            );
            let mut keys = BTreeSet::new();
            for edition in &work.editions {
                assert_eq!(
                    edition.availability,
                    Availability::Owned,
                    "a window edition that is not owned was accepted"
                );
                assert!(
                    !edition.metadata.title.trim().is_empty(),
                    "a blank title accepted"
                );
                assert!(
                    keys.insert((&edition.store, &edition.marketplace, &edition.product_id)),
                    "an edition listed twice was accepted"
                );
            }
        }
    }
}

/// `frame` as a JSON tree.
fn json(frame: &Frame) -> Value {
    serde_json::from_str(&frame.to_text()).unwrap_or(Value::Null)
}

/// True when `a` and `b` are the same tree, floats equal to within a few
/// rounding steps.
fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) if x.is_f64() || y.is_f64() => {
            match (x.as_f64(), y.as_f64()) {
                (Some(x), Some(y)) => {
                    x == y || (x - y).abs() <= x.abs().max(y.abs()) * 4.0 * f64::EPSILON
                }
                _ => false,
            }
        }
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(x, y)| same(x, y))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(key, x)| y.get(key).is_some_and(|y| same(x, y)))
        }
        _ => a == b,
    }
}
