//! Identity of a live socket-proxy endpoint.
//!
//! The tag is the answer from the server that owns the pipe. A numeric fd or
//! handle in some other process is not that identity. `tags` is every channel
//! the probe actually reached, including an inherited endpoint besides the
//! configured one.

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
    use super::foreign_channel_absent;

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
