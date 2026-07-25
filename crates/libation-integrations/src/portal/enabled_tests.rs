//! Portal appearance / actions follow plugin `enabled` flags.

use libation_config::Config;
use libation_source::ContentSource;

use super::brands::Brand;
use super::html::{credential_login_brands, landing_page};

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

fn brand_for(id: &str) -> Brand {
    match id {
        "audible" => Brand::from(libation_audible::AudibleSource::new().portal_brand()),
        "libro" => Brand {
            id: "libro",
            name: "Libro.fm",
            bg: "#1F4E3D",
            fg: "#F4F1EA",
            accent: "#2F6B53",
            icon_url: "https://www.google.com/s2/favicons?domain=libro.fm&sz=128",
        },
        _ => Brand {
            id: "unknown",
            name: "Unknown",
            bg: "#334155",
            fg: "#f8fafc",
            accent: "#64748b",
            icon_url: "",
        },
    }
}

#[test]
fn landing_page_embeds_only_enabled_source_brands() {
    let enabled = [brand_for("audible"), brand_for("libro")];
    let html = landing_page("/connect", &[], &enabled);
    assert!(html.contains("\"audible\""));
    assert!(html.contains("\"libro\""));
    assert!(!html.contains("\"chirp\""));
    assert!(!html.contains("\"graphicaudio\""));
    assert!(!html.contains("\"audiobookshelf\""));
}

#[test]
fn landing_page_embeds_credential_provider_when_enabled() {
    let providers = credential_login_brands(&[String::from("audiobookshelf")]);
    assert_eq!(providers.len(), 1);
    let html = landing_page("/connect", &providers, &[brand_for("audible")]);
    assert!(html.contains("Sign in with Audiobookshelf") || html.contains("audiobookshelf"));
    assert!(html.contains("\"audiobookshelf\""));
    assert!(!html.contains("\"chirp\""));
}

#[test]
fn connections_ui_supports_revoke_when_source_disabled() {
    // Connect buttons omit disabled sources from BRANDS, but the connections
    // list still renders revoke for them (API returns source_enabled: false).
    let html = landing_page("/connect", &[], &[brand_for("audible")]);
    assert!(html.contains("source_enabled"));
    assert!(html.contains("source disabled"));
    assert!(html.contains("/revoke"));
}
