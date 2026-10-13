//! Instance CONFIG schemas derived from guest `describe().config_options`.

use bookclerk_library::control_plane::{
    InstancePackagePolicy, InstanceSettingsSchema, GRAPHICAUDIO_MANIFEST_ID,
};
use bookclerk_plugin_abi::{ConfigOption, ConfigOptionValue, PluginDescribe};

/// Builds a package policy from a describe snapshot.
///
/// Empty `config_options` ⇒ structural-only [`InstancePackagePolicy::Generic`].
#[must_use]
pub fn package_policy_from_describe(describe: &PluginDescribe) -> InstancePackagePolicy {
    package_policy_from_config_options(&describe.config_options)
}

/// Builds a package policy from Cap'n Proto config option rows.
#[must_use]
pub fn package_policy_from_config_options(options: &[ConfigOption]) -> InstancePackagePolicy {
    if options.is_empty() {
        InstancePackagePolicy::Generic
    } else {
        InstancePackagePolicy::Schema(InstanceSettingsSchema::from_config_options(options))
    }
}

/// GraphicAudio writable settings advertised by the guest `describe()`.
///
/// Kept in lockstep with
/// `crates/bookclerk-plugins/optional/source-graphicaudio` `describe()` /
/// `GA_CONFIG_OPTIONS` (plus free-string `base_url` / `store_url`). The host
/// library does not hard-code these keys; enrollment and PUT use this schema
/// (or a live describe snapshot) until a package-declared schema lands.
#[must_use]
pub fn graphicaudio_config_options() -> Vec<ConfigOption> {
    vec![
        ConfigOption {
            key: "access".into(),
            label: "Access".into(),
            values: vec![
                ConfigOptionValue {
                    id: "web".into(),
                    label: "Browser Player".into(),
                },
                ConfigOptionValue {
                    id: "zip".into(),
                    label: "Magento ZIP".into(),
                },
                ConfigOptionValue {
                    id: "device".into(),
                    label: "Access App".into(),
                },
            ],
        },
        ConfigOption {
            key: "base_url".into(),
            label: "Access API base URL".into(),
            values: Vec::new(),
        },
        ConfigOption {
            key: "store_url".into(),
            label: "Store base URL".into(),
            values: Vec::new(),
        },
        ConfigOption {
            key: "bitrate".into(),
            label: "Bitrate".into(),
            values: vec![
                ConfigOptionValue {
                    id: "hi".into(),
                    label: "Hi".into(),
                },
                ConfigOptionValue {
                    id: "lo".into(),
                    label: "Lo".into(),
                },
            ],
        },
        ConfigOption {
            key: "container".into(),
            label: "Container".into(),
            values: vec![
                ConfigOptionValue {
                    id: "auto".into(),
                    label: "Auto".into(),
                },
                ConfigOptionValue {
                    id: "m4b".into(),
                    label: "M4B".into(),
                },
                ConfigOptionValue {
                    id: "mp3".into(),
                    label: "MP3".into(),
                },
                ConfigOptionValue {
                    id: "flac".into(),
                    label: "FLAC".into(),
                },
            ],
        },
    ]
}

/// Policy for the first-party GraphicAudio package.
#[must_use]
pub fn graphicaudio_package_policy() -> InstancePackagePolicy {
    package_policy_from_config_options(&graphicaudio_config_options())
}

/// True when `plugin_key` or manifest id names the GraphicAudio package.
#[must_use]
pub fn is_graphicaudio_package(plugin_key_or_id: &str) -> bool {
    plugin_key_or_id
        .rsplit('/')
        .next()
        .unwrap_or(plugin_key_or_id)
        .eq_ignore_ascii_case(GRAPHICAUDIO_MANIFEST_ID)
        || plugin_key_or_id.eq_ignore_ascii_case(GRAPHICAUDIO_MANIFEST_ID)
}
