//! Network grants derived from plugin settings.
//!
//! A `[[settings]]` entry that declares `network = "fetch"` or `"tcp"` names
//! one destination through its value (the Audiobookshelf server URL, an S3
//! endpoint). Saving the value grants nothing: the host derives the
//! destination ([`bookclerk_plugin_manifest::derive_setting_destination`]),
//! the consent form lists it with the setting as its source, and the
//! operator's approval stores it in [`PluginGrant::setting_destinations`]
//! under the setting key. At spawn, [`apply_setting_destinations`] adds a
//! destination to the egress policy only when the instance's current value
//! derives exactly an approved one.
//!
//! Approving a setting's destinations replaces what was approved for it
//! before ([`approve_setting_destinations`]): destinations no instance uses
//! any more are withdrawn, so grants never pile up as values change.

use std::collections::{BTreeMap, BTreeSet};

use bookclerk_plugin_abi::{SettingNetwork, SettingSpec};
use bookclerk_plugin_manifest::{
    derive_setting_destination, PluginManifest, SettingDestination, SettingDestinationError,
    TcpGrant,
};
use serde_json::Value;

use crate::consent::PluginGrant;

/// Where one network setting's grant stands for one set of values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SettingGrantState {
    /// The value names a destination the operator approved for this setting;
    /// the plugin reaches it.
    Approved,
    /// The value names a destination the operator has not approved yet; the
    /// plugin cannot reach it until the consent form is answered.
    Pending,
    /// The setting is empty or unset: it names no destination.
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

/// One network setting, the destination its value names, and its state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingGrantStatus {
    /// The setting key.
    pub key: String,
    /// The setting's label, falling back to the key.
    pub label: String,
    /// Whether the value is fetched or dialed.
    pub network: SettingNetwork,
    /// The derived destination, when the value names one.
    pub destination: Option<SettingDestination>,
    /// Why the value names no destination, for [`SettingGrantState::Invalid`].
    pub error: Option<SettingDestinationError>,
    /// Where the grant stands.
    pub state: SettingGrantState,
}

/// The settings of `manifest` that declare `network`, in declaration order.
pub fn network_settings(manifest: &PluginManifest) -> impl Iterator<Item = &SettingSpec> {
    manifest
        .settings
        .iter()
        .filter(|spec| spec.network.is_some())
}

/// Derives the destination each network setting names in `settings`.
///
/// # Arguments
///
/// * `manifest` - The plugin whose `[[settings]]` declare the networks.
/// * `settings` - One instance's effective settings (defaults applied), as
///   the JSON object the guest's `CONFIG` carries.
/// * `grant` - The stored grant, which decides approved against pending.
///
/// # Returns
///
/// One status per network setting, in declaration order.
#[must_use]
pub fn setting_grant_statuses(
    manifest: &PluginManifest,
    settings: &Value,
    grant: Option<&PluginGrant>,
) -> Vec<SettingGrantStatus> {
    network_settings(manifest)
        .filter_map(|spec| {
            let network = spec.network?;
            let value = settings
                .get(&spec.key)
                .and_then(Value::as_str)
                .unwrap_or_default();
            let derived = derive_setting_destination(network, spec.default_port, value);
            let (destination, error, state) = match derived {
                Ok(destination) => {
                    let approved = grant.is_some_and(|grant| {
                        grant
                            .setting_destinations
                            .get(&spec.key)
                            .is_some_and(|set| set.contains(&destination))
                    });
                    let state = if approved {
                        SettingGrantState::Approved
                    } else {
                        SettingGrantState::Pending
                    };
                    (Some(destination), None, state)
                }
                Err(SettingDestinationError::Empty) => (None, None, SettingGrantState::Unset),
                Err(err) => (None, Some(err), SettingGrantState::Invalid),
            };
            Some(SettingGrantStatus {
                key: spec.key.clone(),
                label: spec.label_or_key().to_string(),
                network,
                destination,
                error,
                state,
            })
        })
        .collect()
}

/// The network-setting statuses of one plugin instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceSettingStatuses {
    /// The instance; `None` when the plugin has no instance yet and the
    /// statuses judge the settings' defaults.
    pub instance_id: Option<String>,
    /// One status per network setting, in declaration order.
    pub statuses: Vec<SettingGrantStatus>,
}

/// Per setting key, the destinations a plugin's instances name now.
pub type SettingDestinationMap = BTreeMap<String, BTreeSet<SettingDestination>>;

/// Judges the network settings of every instance of a plugin in the library.
///
/// Each instance's configuration document is read and its defaults applied
/// as at spawn; an instance without a document runs on its defaults, and a
/// plugin with no instance yet is judged on its defaults alone. Nothing is
/// read when the plugin declares no network setting.
///
/// # Arguments
///
/// * `library` - The library holding the instances.
/// * `manifest` - The plugin; its schema supplies the defaults.
/// * `plugin_key` - The plugin's canonical key, which its instances carry.
/// * `grant` - The stored grant, which decides approved against pending.
///
/// # Returns
///
/// One entry per instance, oldest first.
///
/// # Errors
///
/// Returns an error when the instances or a document cannot be read.
pub async fn library_setting_statuses(
    library: &bookclerk_library::LibraryStore,
    manifest: &PluginManifest,
    plugin_key: &str,
    grant: Option<&PluginGrant>,
) -> bookclerk_library::Result<Vec<InstanceSettingStatuses>> {
    use bookclerk_library::control_plane::{
        list_plugin_instances_for_key, load_instance_config, SettingValue,
    };
    if network_settings(manifest).next().is_none() {
        return Ok(Vec::new());
    }
    let mut documents = Vec::new();
    for instance in list_plugin_instances_for_key(library, plugin_key).await? {
        let stored: BTreeMap<String, Value> =
            match load_instance_config(library, &instance.id).await {
                Ok(document) => document
                    .body
                    .settings
                    .iter()
                    .map(|(key, value)| (key.clone(), SettingValue::to_json(value)))
                    .collect(),
                Err(bookclerk_library::LibraryError::NotFound(_)) => BTreeMap::new(),
                Err(err) => return Err(err),
            };
        documents.push((Some(instance.id.to_string()), stored));
    }
    if documents.is_empty() {
        documents.push((None, BTreeMap::new()));
    }
    let schema = manifest.settings_schema();
    Ok(documents
        .into_iter()
        .map(|(instance_id, stored)| {
            let config = Value::Object(
                schema
                    .effective_config(&stored)
                    .config
                    .into_iter()
                    .collect(),
            );
            InstanceSettingStatuses {
                instance_id,
                statuses: setting_grant_statuses(manifest, &config, grant),
            }
        })
        .collect())
}

/// The destinations `rows` name, per setting key, for
/// [`approve_setting_destinations`]'s `in_use`.
#[must_use]
pub fn destinations_in_use(rows: &[InstanceSettingStatuses]) -> SettingDestinationMap {
    let mut out = SettingDestinationMap::new();
    for status in rows.iter().flat_map(|row| &row.statuses) {
        if let Some(destination) = &status.destination {
            out.entry(status.key.clone())
                .or_default()
                .insert(destination.clone());
        }
    }
    out
}

/// Adds every approved destination the instance's settings name to `grant`.
///
/// A fetch destination adds its `host:port`, a TCP destination its host and
/// port, and a name or local-network address its local binding, all to
/// [`PluginGrant::setting_network`]. A
/// destination is added only when the current value derives exactly what
/// the operator approved for that setting. Nothing is added unless the grant
/// is outbound.
///
/// # Arguments
///
/// * `grant` - The covering grant for this spawn (mutated in place).
/// * `manifest` - The plugin being spawned.
/// * `settings` - The instance's effective settings (the spawn `CONFIG`).
///
/// # Returns
///
/// The status of each network setting, for the plugin's feedback.
pub fn apply_setting_destinations(
    grant: &mut PluginGrant,
    manifest: &PluginManifest,
    settings: &Value,
) -> Vec<SettingGrantStatus> {
    let statuses = setting_grant_statuses(manifest, settings, Some(grant));
    if !grant.network_mode.eq_ignore_ascii_case("outbound") {
        return statuses;
    }
    for status in &statuses {
        let (SettingGrantState::Approved, Some(destination)) = (status.state, &status.destination)
        else {
            continue;
        };
        let overlay = &mut grant.setting_network;
        if let Some(entry) = destination.domain_entry() {
            overlay.domains.insert(entry);
        }
        if let Some(tcp) = destination.tcp_grant() {
            merge_tcp(&mut overlay.tcp, tcp);
        }
        if let Some(name) = destination.local_name() {
            overlay.local_names.insert(name);
        }
    }
    statuses
}

/// Adds `grant`'s ports to the entry for its host, or inserts it.
fn merge_tcp(set: &mut BTreeSet<TcpGrant>, grant: TcpGrant) {
    let existing = set.iter().find(|t| t.host == grant.host).cloned();
    let mut ports = existing
        .as_ref()
        .map(|t| t.ports.clone())
        .unwrap_or_default();
    if let Some(old) = existing {
        set.remove(&old);
    }
    for port in grant.ports {
        if !ports.contains(&port) {
            ports.push(port);
        }
    }
    ports.sort_unstable();
    set.insert(TcpGrant {
        host: grant.host,
        ports,
    });
}

/// The approved setting destinations after the operator answers the form.
///
/// For each network setting the result holds what is approved and still in
/// use (`in_use` lists the destinations the instances' current values
/// derive), plus each destination the operator approved now that is in use.
/// Anything else is withdrawn: a value that changed no longer keeps its old
/// destination, and a destination nobody named cannot be approved.
///
/// # Arguments
///
/// * `existing` - The stored grant's approvals.
/// * `manifest` - The plugin; only its network settings are kept.
/// * `in_use` - Per setting key, the destinations the instances name now.
/// * `approve` - Per setting key, the destinations the operator approved now.
///
/// # Returns
///
/// The new [`PluginGrant::setting_destinations`].
#[must_use]
pub fn approve_setting_destinations(
    existing: &BTreeMap<String, BTreeSet<SettingDestination>>,
    manifest: &PluginManifest,
    in_use: &BTreeMap<String, BTreeSet<SettingDestination>>,
    approve: &BTreeMap<String, BTreeSet<SettingDestination>>,
) -> BTreeMap<String, BTreeSet<SettingDestination>> {
    let empty = BTreeSet::new();
    let mut out = BTreeMap::new();
    for spec in network_settings(manifest) {
        let used = in_use.get(&spec.key).unwrap_or(&empty);
        let kept = existing.get(&spec.key).unwrap_or(&empty);
        let now = approve.get(&spec.key).unwrap_or(&empty);
        let set: BTreeSet<SettingDestination> = used
            .iter()
            .filter(|d| kept.contains(d) || now.contains(d))
            .cloned()
            .collect();
        if !set.is_empty() {
            out.insert(spec.key.clone(), set);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    // Test code: unwrap on known-good literals is the assertion.
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn manifest() -> PluginManifest {
        bookclerk_plugin_manifest::parse(
            r#"
api_version = 3
id = "abs_probe"
name = "ABS probe"
version = "0.0.0"
runtime = "workerd"
entrypoints = ["cli"]

[workerd]
compatibility_date = "2026-08-01"
main_module = "index.js"
modules_dir = "modules"

[[modules]]
name = "index.js"
path = "index.js"
type = "js"

[vars]

[capabilities.network]
mode = "outbound"
domains = ["api.example.com"]

[[settings]]
key = "server_url"
label = "Server URL"
type = "string"
network = "fetch"

[[settings]]
key = "db"
type = "string"
network = "tcp"
default_port = 5432

[[settings]]
key = "note"
type = "string"
"#,
        )
        .unwrap()
    }

    fn outbound() -> PluginGrant {
        PluginGrant {
            network_mode: "outbound".into(),
            ..PluginGrant::empty()
        }
    }

    #[test]
    fn only_an_approved_value_reaches_the_policy() {
        let manifest = manifest();
        let settings = serde_json::json!({
            "server_url": "http://abs.home.lan:13378",
            "db": "192.168.1.5",
            "note": "abs.evil.example",
        });
        let mut grant = outbound();
        let statuses = apply_setting_destinations(&mut grant, &manifest, &settings);
        assert_eq!(statuses.len(), 2, "only network settings");
        assert!(statuses
            .iter()
            .all(|s| s.state == SettingGrantState::Pending));
        assert_eq!(
            grant.setting_network,
            crate::consent::SettingNetworkOverlay::default()
        );

        let mut approved = outbound();
        for status in &statuses {
            approved
                .setting_destinations
                .entry(status.key.clone())
                .or_default()
                .insert(status.destination.clone().unwrap());
        }
        let statuses = apply_setting_destinations(&mut approved, &manifest, &settings);
        assert!(statuses
            .iter()
            .all(|s| s.state == SettingGrantState::Approved));
        assert!(approved
            .setting_network
            .domains
            .contains("abs.home.lan:13378"));
        assert!(approved
            .setting_network
            .tcp
            .iter()
            .any(|t| t.host == "192.168.1.5" && t.ports == vec![5432]));
        let policy = approved.egress_policy();
        assert!(policy.allows_fetch("abs.home.lan", 13378));
        assert!(!policy.allows_fetch("abs.home.lan", 443));
        assert!(policy.allows_ip_for("abs.home.lan", "10.1.2.3".parse().unwrap()));
        assert!(policy.allows_tcp("192.168.1.5", 5432));

        // A changed value is pending again; the old approval grants nothing.
        let moved = serde_json::json!({ "server_url": "http://abs2.home.lan:13378" });
        let mut grant = approved.clone();
        grant.setting_network = crate::consent::SettingNetworkOverlay::default();
        let statuses = apply_setting_destinations(&mut grant, &manifest, &moved);
        assert_eq!(statuses[0].state, SettingGrantState::Pending);
        assert_eq!(statuses[1].state, SettingGrantState::Unset);
        assert!(grant.setting_network.domains.is_empty());
    }

    #[test]
    fn deny_mode_adds_nothing_and_bad_values_are_invalid() {
        let manifest = manifest();
        let settings = serde_json::json!({ "server_url": "ftp://x.example", "db": "db.lan" });
        let mut grant = PluginGrant::empty();
        let statuses = apply_setting_destinations(&mut grant, &manifest, &settings);
        assert_eq!(statuses[0].state, SettingGrantState::Invalid);
        assert_eq!(statuses[0].error, Some(SettingDestinationError::Scheme));
        assert_eq!(statuses[1].state, SettingGrantState::Pending);
        grant
            .setting_destinations
            .entry("db".into())
            .or_default()
            .insert(statuses[1].destination.clone().unwrap());
        apply_setting_destinations(&mut grant, &manifest, &settings);
        assert!(
            grant.setting_network.tcp.is_empty(),
            "deny mode grants nothing"
        );
    }

    #[test]
    fn approving_replaces_what_a_setting_had() {
        let manifest = manifest();
        let derive =
            |value: &str| derive_setting_destination(SettingNetwork::Fetch, None, value).unwrap();
        let old = derive("http://abs.home.lan:13378");
        let new = derive("http://abs2.home.lan:13378");
        let stranger = derive("https://never.example");
        let existing = BTreeMap::from([("server_url".to_string(), BTreeSet::from([old.clone()]))]);
        let in_use = BTreeMap::from([("server_url".to_string(), BTreeSet::from([new.clone()]))]);
        let approve = BTreeMap::from([
            (
                "server_url".to_string(),
                BTreeSet::from([new.clone(), stranger]),
            ),
            ("note".to_string(), BTreeSet::from([old.clone()])),
        ]);
        let out = approve_setting_destinations(&existing, &manifest, &in_use, &approve);
        assert_eq!(
            out,
            BTreeMap::from([("server_url".to_string(), BTreeSet::from([new]))])
        );
        // Approving nothing keeps what is approved and still in use.
        let both = BTreeMap::from([("server_url".to_string(), BTreeSet::from([old.clone()]))]);
        let kept = approve_setting_destinations(&existing, &manifest, &both, &BTreeMap::new());
        assert_eq!(kept, both);
    }
}
