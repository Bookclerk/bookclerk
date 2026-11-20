//! The manifest's network grants cover every Audible marketplace.

/// The plugin's manifest, as the host reads it.
const MANIFEST: &str = include_str!("../plugin.toml");

/// Every marketplace audible-rs can sign in to has TCP grants for its
/// Amazon and Audible hosts (token refresh and device deregistration go to
/// `api.amazon.<domain>`, the library API to `api.audible.<domain>`), and
/// those hosts keep the app agent. A non-US account would otherwise be cut
/// off by the egress policy.
#[test]
fn every_marketplace_has_tcp_grants_and_the_app_agent() {
    for locale in audible_rs::api::locale::LOCALES {
        for brand in ["amazon", "audible"] {
            let host = format!("*.{brand}.{}", locale.domain);
            let grant = format!("{{ host = \"{host}\", ports = [443] }}");
            assert!(
                MANIFEST.contains(&grant),
                "no TCP grant for {host} ({})",
                locale.country_code
            );
            assert!(
                MANIFEST.contains(&format!("\"{host}\",")),
                "{host} ({}) does not keep the app agent",
                locale.country_code
            );
        }
    }
}
