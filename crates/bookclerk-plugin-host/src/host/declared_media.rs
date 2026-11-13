//! Drops catalog hits and metadata records whose kinds fall outside the
//! plugin's `MediaSupport` declaration.
//!
//! A scan page with such a title is refused whole (see `scan_pass`), but
//! catalog search and metadata answers are advisory: one bad record is
//! dropped and logged so the rest of the answer still reaches the user.

use bookclerk_plugin_sdk::{ExpressionKind, MediaKind, MediaSupport, WorkKind};

/// Whether a record's kinds are inside `media`; logs and returns false when
/// they are not.
///
/// # Arguments
///
/// * `plugin` - Id of the plugin that answered, for the log line.
/// * `media` - The plugin's declaration (equal to its manifest's `[media]`).
/// * `what` - What the record is (`catalog hit`, `metadata record`).
/// * `kinds` - The record's work kind, expression kind and carrier.
///
/// # Returns
///
/// True when the record names all three kinds and its work kind and
/// carrier are declared.
pub(crate) fn declared(
    plugin: &str,
    media: &MediaSupport,
    what: &str,
    kinds: (WorkKind, ExpressionKind, MediaKind),
) -> bool {
    let (work, expression, carrier) = kinds;
    match bookclerk_plugin_abi::check_record_kinds(media, work, expression, carrier) {
        Ok(()) => true,
        Err(err) => {
            tracing::warn!(plugin, error = %err, "dropping {what} outside the plugin's media");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_declared_kinds_pass() {
        let media = MediaSupport {
            work_kinds: vec![WorkKind::Book],
            lists: vec![MediaKind::Print],
            fetches: Vec::new(),
        };
        let print = (WorkKind::Book, ExpressionKind::Text, MediaKind::Print);
        assert!(declared("p", &media, "metadata record", print));
        let audio = (WorkKind::Book, ExpressionKind::Narration, MediaKind::Audio);
        assert!(!declared("p", &media, "metadata record", audio));
        let unknown = (
            WorkKind::Book,
            ExpressionKind::Unspecified,
            MediaKind::Print,
        );
        assert!(!declared("p", &media, "metadata record", unknown));
    }
}
