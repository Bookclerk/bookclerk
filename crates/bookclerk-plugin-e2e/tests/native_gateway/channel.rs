//! Identity of a live socket-proxy endpoint.
//!
//! The tag is the answer from the server that owns the pipe. A numeric fd or
//! handle in some other process is not that identity. `tags` is every channel
//! the probe actually reached. `extra_status` says whether the one handed
//! extra endpoint was absent, observed, or not identified. A missing status
//! is not absence.

/// `true` when every observed tag is a real answer and none of them is `foreign`.
///
/// An empty observation is not absence: the probe did not exercise an endpoint.
/// One configured tag that differs from `foreign` is absence only for that
/// observed set. An extra inherited tag equal to `foreign` makes this false.
#[must_use]
pub fn foreign_channel_absent(observed: &[&str], foreign: &str) -> bool {
    !foreign.is_empty()
        && !observed.is_empty()
        && observed
            .iter()
            .all(|tag| !tag.is_empty() && *tag != foreign)
}

/// Isolation of `foreign` against one `channel_ident` outcome.
///
/// `Ok(true)` means `extra_status` is `absent`, `extra_error` is empty, and
/// every observed tag is a real answer other than `foreign`. `Ok(false)`
/// means `extra_status` is `observed`, `extra_error` is empty, and `foreign`
/// is one of those tags. Every other shape is an error: discovery did not
/// run, identification failed, the tag set is empty, or the status and the
/// tags disagree. An error is not a successful observation.
pub fn channel_endpoint_isolation(
    outcome: &serde_json::Value,
    foreign: &str,
) -> Result<bool, String> {
    let Some(status) = outcome.get("extra_status").and_then(|value| value.as_str()) else {
        return Err("extra_status is missing; discovery did not run".into());
    };
    if status.is_empty() || status == "not-run" {
        return Err(format!("extra_status `{status}` is not an observation"));
    }
    let extra_error = outcome
        .get("extra_error")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    let tags = observed_channel_tags(outcome);
    let observed: Vec<&str> = tags.iter().map(String::as_str).collect();
    match status {
        "absent" => {
            if !extra_error.is_empty() {
                return Err(format!(
                    "absent observation carried an error: {extra_error}"
                ));
            }
            if foreign_channel_absent(&observed, foreign) {
                Ok(true)
            } else if observed.contains(&foreign) {
                Err(format!(
                    "foreign tag `{foreign}` was present while extra_status was absent"
                ))
            } else {
                Err(format!(
                    "absent observation did not prove `{foreign}` was missing from {observed:?}"
                ))
            }
        }
        "observed" => {
            if !extra_error.is_empty() {
                return Err(format!("observed endpoint carried an error: {extra_error}"));
            }
            if observed.contains(&foreign) {
                Ok(false)
            } else {
                Err(format!(
                    "observed endpoint did not include `{foreign}` in {observed:?}"
                ))
            }
        }
        "ident-failed" => Err(format!("endpoint identification failed: {extra_error}")),
        other => Err(format!("unexpected extra_status `{other}`")),
    }
}

/// Tags the probe reported in `tags`, in order.
#[must_use]
pub fn observed_channel_tags(outcome: &serde_json::Value) -> Vec<String> {
    outcome
        .get("tags")
        .and_then(|tags| tags.as_array())
        .map(|tags| {
            tags.iter()
                .filter_map(|tag| tag.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::{channel_endpoint_isolation, foreign_channel_absent};

    fn outcome(status: &str, error: &str, tags: &[&str]) -> serde_json::Value {
        serde_json::json!({
            "ok": status != "ident-failed",
            "extra_status": status,
            "extra_error": error,
            "tags": tags,
        })
    }

    #[test]
    fn isolation_requires_an_explicit_extra_status() {
        assert_eq!(
            channel_endpoint_isolation(&outcome("absent", "", &["ch-a"]), "ch-b"),
            Ok(true)
        );
        assert_eq!(
            channel_endpoint_isolation(
                &outcome("observed", "", &["ch-a", "endpoint-b"]),
                "endpoint-b"
            ),
            Ok(false)
        );
        assert!(
            channel_endpoint_isolation(&serde_json::json!({"tags": ["ch-a"]}), "ch-b").is_err()
        );
        assert!(channel_endpoint_isolation(&outcome("", "", &["ch-a"]), "ch-b").is_err());
        assert!(channel_endpoint_isolation(&outcome("not-run", "", &["ch-a"]), "ch-b").is_err());
        assert!(
            channel_endpoint_isolation(&outcome("ident-failed", "dup", &["ch-a"]), "ch-b").is_err()
        );
        assert!(
            channel_endpoint_isolation(&outcome("ident-failed", "", &["ch-a"]), "ch-b").is_err()
        );
        assert!(channel_endpoint_isolation(&outcome("absent", "", &[]), "ch-b").is_err());
        assert!(
            channel_endpoint_isolation(&outcome("absent", "stale", &["ch-a"]), "ch-b").is_err()
        );
        assert!(
            channel_endpoint_isolation(&outcome("observed", "", &["ch-a"]), "endpoint-b").is_err()
        );
        assert!(channel_endpoint_isolation(
            &outcome("absent", "", &["ch-a", "endpoint-b"]),
            "endpoint-b"
        )
        .is_err());
        assert!(channel_endpoint_isolation(&outcome("ambiguous", "", &["ch-a"]), "ch-b").is_err());
    }

    #[test]
    fn absence_requires_every_observed_tag() {
        assert!(foreign_channel_absent(&["endpoint-a"], "endpoint-b"));
        assert!(!foreign_channel_absent(
            &["endpoint-a", "endpoint-b"],
            "endpoint-b"
        ));
        assert!(foreign_channel_absent(
            &["endpoint-a", "endpoint-b"],
            "endpoint-z"
        ));
        assert!(!foreign_channel_absent(&[], "endpoint-b"));
        assert!(!foreign_channel_absent(&["endpoint-a"], ""));
        assert!(!foreign_channel_absent(&[""], "endpoint-b"));
    }
}
