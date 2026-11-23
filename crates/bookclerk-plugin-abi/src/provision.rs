//! The checks the host runs on what a plugin's guided setup returns.
//!
//! `Provisioning.provision` hands back settings to store and credentials to
//! seal. Both cross a trust boundary, so the host checks them with
//! [`check_provisioned`] before anything is written:
//!
//! - every key is one the manifest lists in `[provisioning] outputs`, as a
//!   setting or as a secret, and no key comes twice;
//! - settings are a flat JSON object of strings, numbers and booleans;
//! - credentials are non-empty single-line text within
//!   [`MAX_PROVISION_SECRET_BYTES`], at most [`MAX_PROVISION_SECRETS`] of
//!   them;
//! - the summary is a few short printable lines;
//! - neither a returned credential nor the operator's grant shows up in a
//!   setting value or a summary line, since those are stored and shown in
//!   the clear.
//!
//! The host then checks each value against the pinned settings schema. The
//! fuzz target `provision_reply` drives the same code.

use std::collections::{BTreeMap, BTreeSet};

use crate::{
    PluginError, Provisioned, MAX_PROVISION_SECRETS, MAX_PROVISION_SECRET_BYTES,
    MAX_PROVISION_SETTINGS_BYTES, MAX_PROVISION_SUMMARY_LINES, MAX_PROVISION_SUMMARY_LINE_BYTES,
};

/// Longest string a guided setup may store as one setting, in bytes.
pub const MAX_PROVISION_SETTING_VALUE_BYTES: usize = 4096;

/// The keys a plugin's guided setup may write (`[provisioning] outputs`),
/// split by whether the setting is declared `type = "secret"`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProvisionOutputs {
    /// Non-secret setting keys, stored in the instance's settings document.
    pub settings: BTreeSet<String>,
    /// `secret` setting keys, sealed into the secret store.
    pub secrets: BTreeSet<String>,
}

/// A credential a guided setup returned, held for sealing. `Debug` never
/// prints it.
#[derive(Clone, PartialEq, Eq)]
pub struct ProvisionedCredential(String);

impl ProvisionedCredential {
    /// The credential's text, for sealing.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for ProvisionedCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ProvisionedCredential(<redacted>)")
    }
}

/// What a guided setup returned, after [`check_provisioned`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CheckedProvision {
    /// Settings to store, keyed by setting key: strings, numbers or
    /// booleans.
    pub settings: BTreeMap<String, serde_json::Value>,
    /// Credentials to seal, keyed by `secret` setting key.
    pub secrets: BTreeMap<String, ProvisionedCredential>,
    /// Lines for the setup screen.
    pub summary: Vec<String>,
}

/// Why [`check_provisioned`] refused a reply. The text names keys and
/// limits, never a value.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProvisionRefusal {
    /// `settingsJson` is longer than [`MAX_PROVISION_SETTINGS_BYTES`].
    #[error("the settings are longer than {MAX_PROVISION_SETTINGS_BYTES} bytes")]
    SettingsTooLong,
    /// `settingsJson` is not a JSON object, or repeats a key.
    #[error("the settings are not a JSON object of distinct keys")]
    SettingsNotObject,
    /// A setting key is not a non-secret key in `[provisioning] outputs`.
    #[error("setting `{0}` is not a declared output")]
    UndeclaredSetting(String),
    /// A setting value is not a string, number or boolean, or is a string
    /// that is too long or holds a control character.
    #[error("setting `{0}` has a value that cannot be stored")]
    BadSettingValue(String),
    /// More than [`MAX_PROVISION_SECRETS`] credentials.
    #[error("more than {MAX_PROVISION_SECRETS} credentials")]
    TooManySecrets,
    /// A credential key is not a secret key in `[provisioning] outputs`.
    #[error("credential `{0}` is not a declared secret output")]
    UndeclaredSecret(String),
    /// A credential key came twice.
    #[error("credential `{0}` is returned twice")]
    RepeatedSecret(String),
    /// A credential is empty, too long, holds a control character or has
    /// space at either end.
    #[error("credential `{0}` is empty, too long or not a single line")]
    BadSecretValue(String),
    /// More than [`MAX_PROVISION_SUMMARY_LINES`] summary lines.
    #[error("more than {MAX_PROVISION_SUMMARY_LINES} summary lines")]
    TooManySummaryLines,
    /// A summary line is too long or holds a control character.
    #[error("summary line {0} is too long or not printable")]
    BadSummaryLine(usize),
    /// A credential or the grant appears in a setting value or a summary
    /// line, which would store or show it in the clear.
    #[error("a credential appears in a stored setting or the summary")]
    CredentialLeak,
}

impl From<ProvisionRefusal> for PluginError {
    fn from(refusal: ProvisionRefusal) -> Self {
        PluginError::invalid_params(format!("the guided setup's answer: {refusal}"))
    }
}

/// Whether `text` holds a character that does not belong on one line.
fn has_control(text: &str) -> bool {
    text.chars().any(char::is_control)
}

/// Checks a plugin's [`Provisioned`] against its declared outputs.
///
/// # Arguments
///
/// * `provisioned` - What `Provisioning.provision` returned.
/// * `outputs` - The manifest's `[provisioning] outputs`, split into
///   settings and secrets.
/// * `grant_token` - The token the operator gave this run, which must not
///   be stored in the clear; `None` when there was none.
///
/// # Returns
///
/// The settings, credentials and summary, each checked.
///
/// # Errors
///
/// Returns the first [`ProvisionRefusal`] that applies.
pub fn check_provisioned(
    provisioned: &Provisioned,
    outputs: &ProvisionOutputs,
    grant_token: Option<&str>,
) -> Result<CheckedProvision, ProvisionRefusal> {
    let settings = check_settings(&provisioned.settings_json, outputs)?;
    if provisioned.secrets.len() > MAX_PROVISION_SECRETS as usize {
        return Err(ProvisionRefusal::TooManySecrets);
    }
    let mut secrets = BTreeMap::new();
    for secret in &provisioned.secrets {
        if !outputs.secrets.contains(&secret.key) {
            return Err(ProvisionRefusal::UndeclaredSecret(secret.key.clone()));
        }
        let value = &secret.value;
        if value.is_empty()
            || value.len() > MAX_PROVISION_SECRET_BYTES as usize
            || has_control(value)
            || value.trim() != value
        {
            return Err(ProvisionRefusal::BadSecretValue(secret.key.clone()));
        }
        if secrets
            .insert(secret.key.clone(), ProvisionedCredential(value.clone()))
            .is_some()
        {
            return Err(ProvisionRefusal::RepeatedSecret(secret.key.clone()));
        }
    }
    if provisioned.summary.len() > MAX_PROVISION_SUMMARY_LINES as usize {
        return Err(ProvisionRefusal::TooManySummaryLines);
    }
    for (index, line) in provisioned.summary.iter().enumerate() {
        if line.len() > MAX_PROVISION_SUMMARY_LINE_BYTES as usize || has_control(line) {
            return Err(ProvisionRefusal::BadSummaryLine(index));
        }
    }
    let clear: Vec<&str> = settings
        .values()
        .filter_map(serde_json::Value::as_str)
        .chain(provisioned.summary.iter().map(String::as_str))
        .collect();
    let hidden = secrets
        .values()
        .map(ProvisionedCredential::expose)
        .chain(grant_token.filter(|token| !token.is_empty()));
    for credential in hidden {
        if clear.iter().any(|text| text.contains(credential)) {
            return Err(ProvisionRefusal::CredentialLeak);
        }
    }
    Ok(CheckedProvision {
        settings,
        secrets,
        summary: provisioned.summary.clone(),
    })
}

/// Parses and checks `settingsJson`.
///
/// # Errors
///
/// The [`ProvisionRefusal`] for a long, malformed or undeclared setting.
fn check_settings(
    json: &str,
    outputs: &ProvisionOutputs,
) -> Result<BTreeMap<String, serde_json::Value>, ProvisionRefusal> {
    if json.len() > MAX_PROVISION_SETTINGS_BYTES as usize {
        return Err(ProvisionRefusal::SettingsTooLong);
    }
    let DistinctObject(entries) =
        serde_json::from_str(json).map_err(|_| ProvisionRefusal::SettingsNotObject)?;
    for (key, value) in &entries {
        if !outputs.settings.contains(key) {
            return Err(ProvisionRefusal::UndeclaredSetting(key.clone()));
        }
        let storable = match value {
            serde_json::Value::String(text) => {
                text.len() <= MAX_PROVISION_SETTING_VALUE_BYTES && !has_control(text)
            }
            serde_json::Value::Number(_) | serde_json::Value::Bool(_) => true,
            serde_json::Value::Null | serde_json::Value::Array(_) | serde_json::Value::Object(_) => {
                false
            }
        };
        if !storable {
            return Err(ProvisionRefusal::BadSettingValue(key.clone()));
        }
    }
    Ok(entries)
}

/// A JSON object read without letting a repeated key overwrite an earlier
/// one.
struct DistinctObject(BTreeMap<String, serde_json::Value>);

impl<'de> serde::Deserialize<'de> for DistinctObject {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        /// Collects the entries, failing on a repeat.
        struct Entries;

        impl<'de> serde::de::Visitor<'de> for Entries {
            type Value = DistinctObject;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a JSON object")
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Self::Value, A::Error> {
                let mut out = BTreeMap::new();
                while let Some((key, value)) = map.next_entry::<String, serde_json::Value>()? {
                    if out.insert(key, value).is_some() {
                        return Err(serde::de::Error::custom("a key is repeated"));
                    }
                }
                Ok(DistinctObject(out))
            }
        }

        de.deserialize_map(Entries)
    }
}

/// Reads an unpacked Cap'n Proto message with limits sized to `bytes`.
///
/// # Errors
///
/// Returns the Cap'n Proto error the reader reports.
fn read_message(
    bytes: &[u8],
) -> capnp::Result<capnp::message::Reader<capnp::serialize::OwnedSegments>> {
    let words = (bytes.len() / 8).saturating_add(16);
    let mut opts = capnp::message::ReaderOptions::new();
    opts.traversal_limit_in_words(Some(words));
    opts.nesting_limit(32);
    capnp::serialize::read_message(&mut std::io::Cursor::new(bytes), opts)
}

/// Decodes a `ProvisionReply` from an unpacked Cap'n Proto message, the
/// form a plugin's answer crosses the plugin boundary in.
///
/// # Arguments
///
/// * `bytes` - The message bytes.
///
/// # Returns
///
/// The reply: what to store, or the plugin's typed error.
///
/// # Errors
///
/// Returns the Cap'n Proto error the reader reports.
pub fn decode_provision_reply_bytes(
    bytes: &[u8],
) -> capnp::Result<Result<Provisioned, PluginError>> {
    let reader = read_message(bytes)?;
    crate::read_provision_reply(
        reader.get_root::<crate::plugin_capnp::provision_reply::Reader<'_>>()?,
    )
}

/// Encodes a `ProvisionReply`, the inverse of
/// [`decode_provision_reply_bytes`].
///
/// # Arguments
///
/// * `reply` - The reply to encode.
///
/// # Returns
///
/// The message bytes.
///
/// # Errors
///
/// Returns the Cap'n Proto error the builder reports.
pub fn encode_provision_reply_bytes(
    reply: &Result<Provisioned, PluginError>,
) -> capnp::Result<Vec<u8>> {
    let mut message = capnp::message::Builder::new_default();
    crate::write_provision_reply(
        message.init_root::<crate::plugin_capnp::provision_reply::Builder<'_>>(),
        reply,
    )?;
    Ok(capnp::serialize::write_message_to_words(&message))
}

#[cfg(test)]
#[allow(clippy::missing_panics_doc)]
mod tests {
    use super::*;
    use crate::ProvisionedSecret;

    fn outputs() -> ProvisionOutputs {
        ProvisionOutputs {
            settings: ["account_id", "bucket", "port", "tls"]
                .into_iter()
                .map(String::from)
                .collect(),
            secrets: ["access_key_id", "secret_access_key"]
                .into_iter()
                .map(String::from)
                .collect(),
        }
    }

    fn reply(settings: &str, secrets: &[(&str, &str)], summary: &[&str]) -> Provisioned {
        Provisioned {
            settings_json: settings.into(),
            secrets: secrets
                .iter()
                .map(|(key, value)| ProvisionedSecret {
                    key: (*key).into(),
                    value: (*value).into(),
                })
                .collect(),
            summary: summary.iter().map(|line| (*line).to_string()).collect(),
        }
    }

    #[test]
    fn a_well_formed_reply_is_accepted() {
        let checked = check_provisioned(
            &reply(
                r#"{"account_id":"abc","bucket":"books","port":443,"tls":true}"#,
                &[
                    ("access_key_id", "0123456789abcdef"),
                    ("secret_access_key", "fedcba9876543210"),
                ],
                &["Created bucket books"],
            ),
            &outputs(),
            Some("pasted-token-value"),
        )
        .expect("accepted");
        assert_eq!(checked.settings.len(), 4);
        assert_eq!(
            checked.secrets["access_key_id"].expose(),
            "0123456789abcdef"
        );
        assert_eq!(checked.summary, ["Created bucket books"]);
        assert!(!format!("{checked:?}").contains("0123456789abcdef"));
    }

    #[test]
    fn undeclared_and_repeated_keys_are_refused() {
        let outputs = outputs();
        let refuse = |p: Provisioned| check_provisioned(&p, &outputs, None).unwrap_err();
        assert_eq!(
            refuse(reply(r#"{"other":"x"}"#, &[], &[])),
            ProvisionRefusal::UndeclaredSetting("other".into())
        );
        assert_eq!(
            refuse(reply(r#"{"access_key_id":"x"}"#, &[], &[])),
            ProvisionRefusal::UndeclaredSetting("access_key_id".into())
        );
        assert_eq!(
            refuse(reply(r#"{"bucket":"a","bucket":"b"}"#, &[], &[])),
            ProvisionRefusal::SettingsNotObject
        );
        assert_eq!(
            refuse(reply("{}", &[("bucket", "longenough")], &[])),
            ProvisionRefusal::UndeclaredSecret("bucket".into())
        );
        assert_eq!(
            refuse(reply(
                "{}",
                &[("access_key_id", "aaaaaaaa"), ("access_key_id", "bbbbbbbb")],
                &[]
            )),
            ProvisionRefusal::RepeatedSecret("access_key_id".into())
        );
    }

    #[test]
    fn malformed_values_are_refused() {
        let outputs = outputs();
        let refuse = |p: Provisioned| check_provisioned(&p, &outputs, None).unwrap_err();
        assert_eq!(
            refuse(reply("[]", &[], &[])),
            ProvisionRefusal::SettingsNotObject
        );
        assert_eq!(
            refuse(reply(&"x".repeat(20_000), &[], &[])),
            ProvisionRefusal::SettingsTooLong
        );
        for bad in [r#"{"bucket":null}"#, r#"{"bucket":[1]}"#, r#"{"bucket":"a\nb"}"#] {
            assert_eq!(
                refuse(reply(bad, &[], &[])),
                ProvisionRefusal::BadSettingValue("bucket".into()),
                "{bad}"
            );
        }
        for bad in ["", " padded", "two\nlines"] {
            assert_eq!(
                refuse(reply("{}", &[("access_key_id", bad)], &[])),
                ProvisionRefusal::BadSecretValue("access_key_id".into())
            );
        }
        assert_eq!(
            refuse(reply("{}", &[], &["a\u{7}"])),
            ProvisionRefusal::BadSummaryLine(0)
        );
        assert_eq!(
            refuse(reply("{}", &[], &["x"; 9])),
            ProvisionRefusal::TooManySummaryLines
        );
        let many: Vec<(String, String)> = (0..17)
            .map(|i| ("access_key_id".to_string(), format!("value{i}")))
            .collect();
        let many: Vec<(&str, &str)> = many.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        assert_eq!(
            refuse(reply("{}", &many, &[])),
            ProvisionRefusal::TooManySecrets
        );
    }

    #[test]
    fn credentials_never_land_in_the_clear() {
        let outputs = outputs();
        assert_eq!(
            check_provisioned(
                &reply(
                    r#"{"bucket":"key-0123456789"}"#,
                    &[("access_key_id", "0123456789")],
                    &[]
                ),
                &outputs,
                None
            )
            .unwrap_err(),
            ProvisionRefusal::CredentialLeak
        );
        assert_eq!(
            check_provisioned(
                &reply("{}", &[], &["Token pasted-token was used"]),
                &outputs,
                Some("pasted-token")
            )
            .unwrap_err(),
            ProvisionRefusal::CredentialLeak
        );
        // The grant may come back as a declared credential.
        assert!(check_provisioned(
            &reply("{}", &[("secret_access_key", "pasted-token")], &[]),
            &outputs,
            Some("pasted-token")
        )
        .is_ok());
    }

    #[test]
    fn replies_round_trip_through_the_codec() {
        let ok = Ok(reply(
            r#"{"bucket":"books"}"#,
            &[("access_key_id", "abcdefgh")],
            &["done"],
        ));
        let bytes = encode_provision_reply_bytes(&ok).expect("encode");
        assert_eq!(decode_provision_reply_bytes(&bytes).expect("decode"), ok);
        let err = Err(PluginError::forbidden("no"));
        let bytes = encode_provision_reply_bytes(&err).expect("encode");
        assert_eq!(decode_provision_reply_bytes(&bytes).expect("decode"), err);
        assert!(decode_provision_reply_bytes(&[1, 2, 3]).is_err());
        let refusal: PluginError = ProvisionRefusal::CredentialLeak.into();
        assert_eq!(refusal.code, crate::PluginErrorCode::InvalidParams);
    }
}
