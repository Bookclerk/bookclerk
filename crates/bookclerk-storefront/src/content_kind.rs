//! Content-kind labels storefronts report for podcast shows and episodes.
//!
//! The host's library model re-exports these so a storefront guest and the
//! host classify a `content_kind` string the same way.

/// True when `content_kind` is a podcast episode.
#[must_use]
pub fn is_episode(content_kind: &str) -> bool {
    content_kind.eq_ignore_ascii_case("episode")
}

/// True when `content_kind` is a podcast parent / show (no audio to acquire).
#[must_use]
pub fn is_podcast_parent(content_kind: &str) -> bool {
    matches!(
        content_kind.to_ascii_lowercase().as_str(),
        "podcast" | "parent" | "podcastparent" | "podcast_parent" | "season"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn episode_matches_case_insensitively() {
        assert!(is_episode("Episode"));
        assert!(!is_episode("product"));
    }

    #[test]
    fn podcast_parent_accepts_every_alias() {
        for label in [
            "podcast",
            "Parent",
            "PodcastParent",
            "podcast_parent",
            "season",
        ] {
            assert!(is_podcast_parent(label), "{label}");
        }
        assert!(!is_podcast_parent("episode"));
    }
}
