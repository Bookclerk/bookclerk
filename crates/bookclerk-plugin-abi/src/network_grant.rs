//! The network grant a guest sees at `open` (`Bindings.network`).
//!
//! Read-only feedback: the host enforces the grant whatever the guest does
//! with it. A guest uses it to tell an unapproved destination (a network
//! setting whose value awaits the operator's consent) from one that is
//! simply unreachable, and to say so to the operator.

use serde::{Deserialize, Serialize};

use crate::settings_schema::SettingNetwork;

/// Most entries in each list of a [`NetworkGrant`] (schema
/// `maxNetworkGrantEntries`).
pub const MAX_NETWORK_GRANT_ENTRIES: usize = 256;

/// Where one network setting's destination stands.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum SettingGrantState {
    /// The value names a destination the operator approved for this setting;
    /// the plugin reaches it.
    Approved,
    /// The value names a destination the operator has not approved yet; the
    /// plugin cannot reach it until the consent form is answered.
    Pending,
    /// The setting is empty or unset: it names no destination.
    #[default]
    Unset,
    /// The value does not name a usable destination.
    Invalid,
}

impl SettingGrantState {
    /// Wire name (`approved`, `pending`, `unset`, `invalid`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Approved => "approved",
            Self::Pending => "pending",
            Self::Unset => "unset",
            Self::Invalid => "invalid",
        }
    }
}

/// One network setting and the destination its value names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingNetworkGrant {
    /// The setting key.
    pub key: String,
    /// Whether the plugin fetches the destination or opens a socket to it.
    pub network: SettingNetwork,
    /// The destination as the consent form shows it: `scheme://host:port`
    /// for a fetch, `host:port` for TCP (IPv6 in brackets); empty when
    /// [`Self::state`] is unset or invalid.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub destination: String,
    /// Where the destination stands.
    pub state: SettingGrantState,
    /// Why the value names no destination, when [`Self::state`] is invalid;
    /// empty otherwise.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
}

/// The network grant in effect for one `PluginWorker.open`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkGrant {
    /// True when the grant allows outbound connections; false for `deny`.
    #[serde(default)]
    pub outbound: bool,
    /// Fetch destinations in effect, as `plugin.toml` `domains` entries,
    /// including approved setting destinations.
    #[serde(default)]
    pub domains: Vec<String>,
    /// Raw TCP destinations in effect, as `host:port`, one per granted port.
    #[serde(default)]
    pub tcp: Vec<String>,
    /// One entry per network setting, in declaration order.
    #[serde(default)]
    pub settings: Vec<SettingNetworkGrant>,
}

impl NetworkGrant {
    /// The network setting whose value names `destination` (as
    /// [`SettingNetworkGrant::destination`] spells it), if one does.
    #[must_use]
    pub fn setting_for(&self, destination: &str) -> Option<&SettingNetworkGrant> {
        self.settings
            .iter()
            .find(|setting| !setting.destination.is_empty() && setting.destination == destination)
    }

    /// Keeps at most [`MAX_NETWORK_GRANT_ENTRIES`] entries in each list, so
    /// the grant always fits the schema's bound.
    pub fn truncate_to_bound(&mut self) {
        self.domains.truncate(MAX_NETWORK_GRANT_ENTRIES);
        self.tcp.truncate(MAX_NETWORK_GRANT_ENTRIES);
        self.settings.truncate(MAX_NETWORK_GRANT_ENTRIES);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_destination_maps_back_to_its_setting() {
        let mut grant = NetworkGrant {
            outbound: true,
            settings: vec![
                SettingNetworkGrant {
                    key: "server_url".into(),
                    network: SettingNetwork::Fetch,
                    destination: "abs.home.lan:13378".into(),
                    state: SettingGrantState::Pending,
                    error: String::new(),
                },
                SettingNetworkGrant {
                    key: "db".into(),
                    network: SettingNetwork::Tcp,
                    destination: String::new(),
                    state: SettingGrantState::Unset,
                    error: String::new(),
                },
            ],
            ..NetworkGrant::default()
        };
        assert_eq!(
            grant
                .setting_for("abs.home.lan:13378")
                .map(|s| s.key.as_str()),
            Some("server_url")
        );
        assert!(grant.setting_for("").is_none());
        grant.domains = vec!["a.example".into(); MAX_NETWORK_GRANT_ENTRIES + 3];
        grant.truncate_to_bound();
        assert_eq!(grant.domains.len(), MAX_NETWORK_GRANT_ENTRIES);
        let json = serde_json::to_value(&grant.settings[1]).unwrap_or_default();
        assert_eq!(
            json,
            serde_json::json!({"key": "db", "network": "tcp", "state": "unset"})
        );
    }
}
