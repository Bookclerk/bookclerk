//! Portal appearance / actions follow plugin `enabled` flags.

use bookclerk_config::Config;

#[test]
fn sources_is_enabled_matches_plugin_tables() {
    let mut cfg = Config::default();
    assert!(cfg.sources.is_enabled("audible"));
    assert!(cfg.sources.is_enabled("libro"));
    cfg.sources.set_enabled("chirp", false);
    cfg.sources.set_enabled("graphicaudio", false);
    assert!(!cfg.sources.is_enabled("chirp"));
    assert!(!cfg.sources.is_enabled("graphicaudio"));
    assert!(cfg.sources.is_enabled("audible"));
}

#[test]
fn integrations_is_enabled_defaults_off_for_abs() {
    let mut cfg = Config::default();
    assert!(!cfg.integrations.is_enabled("audiobookshelf"));
    assert!(!cfg.integrations.is_enabled("abs"));
    cfg.integrations.audiobookshelf.enabled = true;
    assert!(cfg.integrations.is_enabled("audiobookshelf"));
    assert!(!cfg.integrations.is_enabled("unknown"));
}
