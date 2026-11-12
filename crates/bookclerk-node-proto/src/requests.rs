//! Interlibrary requests: a member of a library asks for a book one of its
//! branches listed (`requests.claim`, `requests.claimed`,
//! `requests.finish`).
//!
//! A request is a job on the library, pinned to the branch that listed the
//! book. The library wakes the branch; the branch pulls the requests
//! waiting for it, because a wake is a hint and not state:
//!
//! 1. branch → library: [`crate::Body::RequestsClaim`]; answered with
//!    [`crate::Body::RequestsClaimed`], at most [`MAX_CLAIMED_REQUESTS`]
//!    requests (none when nothing waits), each now leased to the branch
//!    under its job id and lease generation.
//! 2. branch → library: [`crate::Body::RequestFinish`] with the
//!    [`RequestOutcome`] for one claimed request; answered with
//!    [`crate::Body::Accepted`], or [`crate::Body::Lost`] when the lease is
//!    no longer the branch's.
//!
//! A request names the work the way a catalog question does
//! ([`LookupWork`], by strong identifiers, never a title) plus the store
//! listing the branch answered with. It carries no account, user or
//! library field, and [`BranchRequest`] refuses unknown fields: the branch
//! decides from its own library which of its people's copies to send.

use serde::{Deserialize, Serialize};

use crate::catalog::{is_product_id, is_store_id};
use crate::lookup::{is_marketplace, LookupWork};

/// Most requests one [`crate::Body::RequestsClaimed`] may carry.
pub const MAX_CLAIMED_REQUESTS: usize = 16;

/// Longest job id a request may carry.
pub const MAX_REQUEST_JOB_BYTES: usize = 96;

/// The work item name of a request in a [`crate::Body::Lost`] answer.
pub const ITEM_REQUEST: &str = "request";

/// True for a request's job id: 1 to [`MAX_REQUEST_JOB_BYTES`] ASCII
/// letters, digits, `-` and `_`.
#[must_use]
pub fn is_request_job(job: &str) -> bool {
    !job.is_empty()
        && job.len() <= MAX_REQUEST_JOB_BYTES
        && job
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// One request a branch claimed: "send this library that book".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BranchRequest {
    /// The library's job id; the branch echoes it when it finishes.
    pub job: String,
    /// The job's lease generation; together with the job id and the
    /// branch's node id it is the fence the finish is checked against.
    pub generation: i64,
    /// The work, by identifier, as a catalog question names it.
    pub work: LookupWork,
    /// The store of the listing the branch answered with, as its plain id
    /// (`audible`).
    pub store: String,
    /// Lower-cased marketplace of that listing; empty when the store has
    /// none.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub marketplace: String,
    /// The store's product id of that listing.
    pub product_id: String,
}

impl BranchRequest {
    /// Why the request is malformed, or `None`: a job id
    /// ([`is_request_job`]), a positive generation, a well-formed work, and
    /// a store id, marketplace and product id within the catalog rules.
    #[must_use]
    pub fn problem(&self) -> Option<&'static str> {
        if let Some(problem) = finish_problem(&self.job, self.generation) {
            return Some(problem);
        }
        if let Some(problem) = self.work.problem() {
            return Some(problem);
        }
        if !is_store_id(&self.store) {
            return Some("invalid_store");
        }
        if !is_marketplace(&self.marketplace) {
            return Some("invalid_marketplace");
        }
        if !is_product_id(&self.product_id) {
            return Some("invalid_product_id");
        }
        None
    }
}

/// The problem with a `requests.finish {job, generation}` a library
/// received, or `None` when both are ones it could have handed out.
#[must_use]
pub fn finish_problem(job: &str, generation: i64) -> Option<&'static str> {
    if !is_request_job(job) {
        return Some("invalid_job");
    }
    if generation < 1 {
        return Some("invalid_generation");
    }
    None
}

/// What a branch did with a claimed request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestOutcome {
    /// The branch took it: the book is queued or already on the branch,
    /// and reaches the library through the branch's catalog sync.
    Accepted,
    /// The branch no longer lists the book to this library (nobody here
    /// holds it, the holder opted out, or sharing is off). Final: the
    /// library does not ask again.
    NotListed,
    /// The branch could not take it now (a local read or write failed).
    /// The library retries it while the job has attempts left.
    Failed,
}

impl RequestOutcome {
    /// The outcome as the library records it on the job.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::NotListed => "not_listed",
            Self::Failed => "failed",
        }
    }
}

/// Why a list of claimed requests is malformed, or `None`: at most
/// [`MAX_CLAIMED_REQUESTS`] well-formed requests, each for a different job.
#[must_use]
pub fn claimed_problem(requests: &[BranchRequest]) -> Option<&'static str> {
    if requests.len() > MAX_CLAIMED_REQUESTS {
        return Some("too_many_requests");
    }
    let mut seen = std::collections::BTreeSet::new();
    for request in requests {
        if let Some(problem) = request.problem() {
            return Some(problem);
        }
        if !seen.insert(request.job.as_str()) {
            return Some("duplicate_job");
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lookup::{LookupIdentifier, LookupScheme};

    fn request() -> BranchRequest {
        BranchRequest {
            job: "branch_request-0b8f".into(),
            generation: 1,
            work: LookupWork {
                work: "0b8f-work".into(),
                identifiers: vec![LookupIdentifier {
                    scheme: LookupScheme::Asin,
                    value: "B0TEST".into(),
                    marketplace: "us".into(),
                }],
            },
            store: "audible".into(),
            marketplace: "us".into(),
            product_id: "B0TEST".into(),
        }
    }

    #[test]
    fn a_request_names_no_account_or_title() {
        let ok = request();
        assert_eq!(ok.problem(), None);
        for field in ["account", "account_id", "user", "title", "library"] {
            let mut value = serde_json::to_value(&ok).unwrap();
            value[field] = serde_json::json!("x");
            assert!(
                serde_json::from_value::<BranchRequest>(value).is_err(),
                "{field} parsed"
            );
        }
    }

    #[test]
    fn requests_are_validated() {
        let mut bad = request();
        bad.job = "job with space".into();
        assert_eq!(bad.problem(), Some("invalid_job"));
        let mut bad = request();
        bad.job = "j".repeat(MAX_REQUEST_JOB_BYTES + 1);
        assert_eq!(bad.problem(), Some("invalid_job"));
        let mut bad = request();
        bad.generation = 0;
        assert_eq!(bad.problem(), Some("invalid_generation"));
        let mut bad = request();
        bad.work.identifiers.clear();
        assert_eq!(bad.problem(), Some("invalid_identifiers"));
        let mut bad = request();
        bad.store = "Audible".into();
        assert_eq!(bad.problem(), Some("invalid_store"));
        let mut bad = request();
        bad.marketplace = "US".into();
        assert_eq!(bad.problem(), Some("invalid_marketplace"));
        let mut bad = request();
        bad.product_id = " B0".into();
        assert_eq!(bad.problem(), Some("invalid_product_id"));
        let mut none = request();
        none.marketplace.clear();
        assert_eq!(none.problem(), None);
    }

    #[test]
    fn claimed_lists_are_bounded_and_distinct() {
        assert_eq!(claimed_problem(&[]), None);
        assert_eq!(claimed_problem(&[request()]), None);
        assert_eq!(
            claimed_problem(&[request(), request()]),
            Some("duplicate_job")
        );
        let many: Vec<BranchRequest> = (0..=MAX_CLAIMED_REQUESTS)
            .map(|i| BranchRequest {
                job: format!("branch_request-{i}"),
                ..request()
            })
            .collect();
        assert_eq!(claimed_problem(&many), Some("too_many_requests"));
        assert_eq!(claimed_problem(&many[1..]), None);
    }

    #[test]
    fn request_frames_round_trip_under_their_names() {
        use crate::{Body, Frame};
        for (body, name) in [
            (Body::RequestsClaim {}, "requests.claim"),
            (
                Body::RequestsClaimed {
                    requests: vec![request()],
                },
                "requests.claimed",
            ),
            (
                Body::RequestFinish {
                    job: "branch_request-0b8f".into(),
                    generation: 2,
                    outcome: RequestOutcome::NotListed,
                },
                "requests.finish",
            ),
        ] {
            let text = Frame::new("f1", body.clone()).to_text();
            let value: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert_eq!(value["type"], name);
            assert_eq!(Frame::parse(&text).unwrap().body, body);
        }
    }

    #[test]
    fn outcomes_have_stable_names() {
        for (outcome, name) in [
            (RequestOutcome::Accepted, "accepted"),
            (RequestOutcome::NotListed, "not_listed"),
            (RequestOutcome::Failed, "failed"),
        ] {
            assert_eq!(outcome.as_str(), name);
            assert_eq!(
                serde_json::to_value(outcome).unwrap(),
                serde_json::json!(name)
            );
        }
    }
}
