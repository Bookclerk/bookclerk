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
//! - a `catalog.ask` or `catalog.found` frame the library or branch would
//!   accept (`questions_problem` / `found_problem` find nothing) names at
//!   most 100 works, each once, with plain correlation keys; every
//!   identifier is plain ASCII with a marketplace exactly when it is an
//!   ASIN; and every answer lists 1 to 8 distinct editions with a
//!   non-blank title. Each is checked byte by byte here, not through the
//!   validators, since the library writes them into its listing pool;
//! - a `requests.claimed` frame the branch would accept (`claimed_problem`
//!   finds nothing) carries at most 16 requests, each for a different job
//!   with a plain job id and a positive lease generation, a question that
//!   meets the same identifier rules, a plain store id, a lowercase
//!   marketplace and a product id with no control characters or outer
//!   whitespace. Checked byte by byte here too, since the branch looks the
//!   request up in its own library and records it.

use std::collections::BTreeSet;

use bookclerk_node_proto::{
    claimed_problem, found_problem, questions_problem, Body, Frame, LookupScheme, LookupWork,
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
    check_lookup(&frame.body);
    check_requests(&frame.body);
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

/// The identifier rules of one accepted question.
fn check_question(work: &LookupWork) {
    assert!(plain(&work.work, 64), "work ref {:?} accepted", work.work);
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
        assert!(
            id.marketplace.len() <= 16
                && id
                    .marketplace
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()),
            "marketplace {:?} accepted",
            id.marketplace
        );
    }
}

/// The interlibrary request invariants on an accepted claimed list.
fn check_requests(body: &Body) {
    let Body::RequestsClaimed { requests } = body else {
        return;
    };
    if claimed_problem(requests).is_some() {
        return;
    }
    assert!(requests.len() <= 16, "too many requests accepted");
    let mut jobs = BTreeSet::new();
    for request in requests {
        assert!(
            !request.job.is_empty()
                && request.job.len() <= 96
                && request
                    .job
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
            "job id {:?} accepted",
            request.job
        );
        assert!(
            jobs.insert(&request.job),
            "a job claimed twice was accepted"
        );
        assert!(
            request.generation >= 1,
            "generation {} accepted",
            request.generation
        );
        check_question(&request.work);
        assert!(
            !request.store.is_empty()
                && request.store.len() <= 64
                && request
                    .store
                    .bytes()
                    .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-' | b'.')),
            "store {:?} accepted",
            request.store
        );
        assert!(
            request.marketplace.len() <= 16
                && request
                    .marketplace
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()),
            "marketplace {:?} accepted",
            request.marketplace
        );
        let product = request.product_id.as_str();
        assert!(
            !product.is_empty()
                && product.len() <= 256
                && product.trim() == product
                && !product.chars().any(char::is_control),
            "product id {product:?} accepted"
        );
    }
}

/// The catalog lookup invariants on an accepted question or answer list.
fn check_lookup(body: &Body) {
    match body {
        Body::CatalogAsk { works } if questions_problem(works).is_none() => {
            assert!(works.len() <= 100, "too many questions accepted");
            let mut seen = BTreeSet::new();
            for work in works {
                check_question(work);
                assert!(seen.insert(&work.work), "a work asked twice was accepted");
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
