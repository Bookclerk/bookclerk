//! On a library: asking the branches that joined it which of the works its
//! people are looking at they can supply (`control_plane::catalog_lookup`).
//!
//! Each Discover candidate fetch ends with [`ask_branches_if_due`]: at most
//! once per [`ASK_INTERVAL`] per daemon, the works of open wishes and
//! Discover candidates are asked about. Asking is idempotent (a question
//! already waiting, or answered within a day, is left alone), so the
//! interval only bounds the reads.

use std::sync::atomic::{AtomicI64, Ordering};

use bookclerk_library::control_plane::{ask_branches, works_of_interest};
use bookclerk_library::LibraryStore;
use chrono::Utc;

/// Least time between two asks on one daemon.
pub const ASK_INTERVAL: chrono::Duration = chrono::Duration::minutes(15);

/// Most works one ask names.
const MAX_ASKED_WORKS: u64 = 2000;

/// Unix seconds of this daemon's last ask (0: never).
static LAST_ASK: AtomicI64 = AtomicI64::new(0);

/// Whether this daemon should ask now, claiming the slot when it should so
/// concurrent jobs do not ask together.
fn ask_due(now: chrono::DateTime<Utc>) -> bool {
    let now_secs = now.timestamp();
    let last = LAST_ASK.load(Ordering::Relaxed);
    now_secs.saturating_sub(last) >= ASK_INTERVAL.num_seconds()
        && LAST_ASK
            .compare_exchange(last, now_secs, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
}

/// Asks this library's branches about the works of open wishes and Discover
/// candidates, when [`ASK_INTERVAL`] has passed since this daemon last did.
/// A failure is logged; the next fetch asks again.
pub async fn ask_branches_if_due(library: &LibraryStore) {
    if !ask_due(Utc::now()) {
        return;
    }
    let works = match works_of_interest(library, MAX_ASKED_WORKS).await {
        Ok(works) => works,
        Err(err) => {
            tracing::warn!(error = %err, "could not read the works to ask branches about");
            return;
        }
    };
    match ask_branches(library, &works).await {
        Ok(report) if report.asked > 0 => tracing::debug!(
            branches = report.branches,
            asked = report.asked,
            "asked branches about works"
        ),
        Ok(_) => {}
        Err(err) => tracing::warn!(error = %err, "could not ask branches about works"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_ask_per_interval() {
        let now = Utc::now() + chrono::Duration::days(1);
        assert!(ask_due(now));
        assert!(!ask_due(now));
        assert!(ask_due(now + ASK_INTERVAL));
    }
}
