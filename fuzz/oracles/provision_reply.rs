//! Shared oracle for the `provision_reply` fuzz target and the
//! `bookclerk-plugin-abi` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The input is a plugin's answer to `Provisioning.provision`. Byte 0 picks
//! the form and the run:
//!
//! - Bit 0 clear: the rest is a Cap'n Proto `ProvisionReply` message, read
//!   as the host reads it off the wire; set: the rest is split on NUL into
//!   the settings JSON, credential key and value pairs up to a lone `--`,
//!   then summary lines, so the checker sees structured answers the
//!   message form rarely reaches.
//! - Bit 1: whether the run had a grant token (`grant-token-xyz`), which
//!   must never be stored or shown.
//!
//! The invariants:
//!
//! - decoding and checking never panic and answer the same twice;
//! - a decoded reply encodes and decodes back to itself;
//! - an accepted answer stores only declared, distinct keys: settings are
//!   strings within the setting cap without control characters, numbers
//!   or booleans, and agree with an independent `serde_json::Value` read of
//!   the same JSON; credentials are non-empty, single-line, trimmed and
//!   within their cap, at most the credential cap of them; the summary is
//!   within its line count and line length without control characters;
//!   and no credential or grant token appears in any setting or summary
//!   line;
//! - a refusal names a key that really breaks the rule it reports.

use bookclerk_plugin_abi::provision::{
    check_provisioned, decode_provision_reply_bytes, encode_provision_reply_bytes,
    ProvisionOutputs, ProvisionRefusal, MAX_PROVISION_SETTING_VALUE_BYTES,
};
use bookclerk_plugin_abi::{
    Provisioned, ProvisionedSecret, MAX_PROVISION_SECRETS, MAX_PROVISION_SECRET_BYTES,
    MAX_PROVISION_SETTINGS_BYTES, MAX_PROVISION_SUMMARY_LINES, MAX_PROVISION_SUMMARY_LINE_BYTES,
};

/// The grant token a run may carry.
const GRANT: &str = "grant-token-xyz";

/// The outputs every answer is checked against (the `r2` plugin's).
fn outputs() -> ProvisionOutputs {
    ProvisionOutputs {
        settings: ["account_id", "bucket", "jurisdiction"]
            .into_iter()
            .map(String::from)
            .collect(),
        secrets: ["access_key_id", "secret_access_key"]
            .into_iter()
            .map(String::from)
            .collect(),
    }
}

/// Checks one input; panics when an invariant fails.
pub fn check(data: &[u8]) {
    let Some((&selector, rest)) = data.split_first() else {
        return;
    };
    let grant = (selector & 2 != 0).then_some(GRANT);
    let provisioned = if selector & 1 == 0 {
        let first = decode_provision_reply_bytes(rest);
        let second = decode_provision_reply_bytes(rest);
        assert_eq!(
            format!("{first:?}"),
            format!("{second:?}"),
            "decoding is deterministic"
        );
        let Ok(reply) = first else {
            return;
        };
        let bytes = encode_provision_reply_bytes(&reply).expect("a decoded reply encodes");
        let again = decode_provision_reply_bytes(&bytes).expect("an encoded reply decodes");
        assert_eq!(
            format!("{reply:?}"),
            format!("{again:?}"),
            "the reply round-trips"
        );
        let Ok(provisioned) = reply else {
            return;
        };
        provisioned
    } else {
        structured(rest)
    };
    check_answer(&provisioned, grant);
}

/// Builds an answer from NUL-separated parts.
fn structured(rest: &[u8]) -> Provisioned {
    let mut parts = rest
        .split(|b| *b == 0)
        .map(|part| String::from_utf8_lossy(part).into_owned());
    let settings_json = parts.next().unwrap_or_default();
    let mut secrets = Vec::new();
    let mut summary = Vec::new();
    let mut in_summary = false;
    while let Some(part) = parts.next() {
        if in_summary {
            summary.push(part);
        } else if part == "--" {
            in_summary = true;
        } else {
            secrets.push(ProvisionedSecret {
                key: part,
                value: parts.next().unwrap_or_default(),
            });
        }
    }
    Provisioned {
        settings_json,
        secrets,
        summary,
    }
}

/// Whether `text` holds a character that does not belong on one line.
fn control(text: &str) -> bool {
    text.chars().any(char::is_control)
}

/// Checks the host's verdict on one answer.
fn check_answer(provisioned: &Provisioned, grant: Option<&str>) {
    let outputs = outputs();
    let first = check_provisioned(provisioned, &outputs, grant);
    let second = check_provisioned(provisioned, &outputs, grant);
    assert_eq!(first, second, "checking is deterministic");
    match first {
        Ok(checked) => {
            assert!(provisioned.settings_json.len() <= MAX_PROVISION_SETTINGS_BYTES as usize);
            let value: serde_json::Value = serde_json::from_str(&provisioned.settings_json)
                .expect("accepted settings are JSON");
            let object = value.as_object().expect("accepted settings are an object");
            assert_eq!(object.len(), checked.settings.len(), "keys are distinct");
            for (key, value) in object {
                assert!(outputs.settings.contains(key), "{key} is declared");
                assert_eq!(checked.settings.get(key), Some(value), "{key} agrees");
                match value {
                    serde_json::Value::String(text) => {
                        assert!(text.len() <= MAX_PROVISION_SETTING_VALUE_BYTES);
                        assert!(!control(text));
                    }
                    serde_json::Value::Number(_) | serde_json::Value::Bool(_) => {}
                    other => panic!("{key} stores {other}"),
                }
            }
            assert!(provisioned.secrets.len() <= MAX_PROVISION_SECRETS as usize);
            assert_eq!(
                checked.secrets.len(),
                provisioned.secrets.len(),
                "credentials are distinct"
            );
            for secret in &provisioned.secrets {
                assert!(outputs.secrets.contains(&secret.key));
                let value = &secret.value;
                assert!(!value.is_empty() && value.len() <= MAX_PROVISION_SECRET_BYTES as usize);
                assert!(!control(value) && value.trim() == value);
                assert_eq!(
                    checked.secrets.get(&secret.key).map(|c| c.expose()),
                    Some(value.as_str())
                );
            }
            assert!(provisioned.summary.len() <= MAX_PROVISION_SUMMARY_LINES as usize);
            assert_eq!(checked.summary, provisioned.summary);
            let clear: Vec<&str> = object
                .values()
                .filter_map(serde_json::Value::as_str)
                .chain(provisioned.summary.iter().map(String::as_str))
                .collect();
            for line in &provisioned.summary {
                assert!(line.len() <= MAX_PROVISION_SUMMARY_LINE_BYTES as usize && !control(line));
            }
            for hidden in provisioned
                .secrets
                .iter()
                .map(|s| s.value.as_str())
                .chain(grant)
            {
                assert!(
                    !clear.iter().any(|text| text.contains(hidden)),
                    "a credential is in the clear"
                );
            }
        }
        Err(ProvisionRefusal::UndeclaredSetting(key)) => {
            assert!(!outputs.settings.contains(&key));
        }
        Err(ProvisionRefusal::UndeclaredSecret(key)) => {
            assert!(!outputs.secrets.contains(&key));
            assert!(provisioned.secrets.iter().any(|s| s.key == key));
        }
        Err(ProvisionRefusal::RepeatedSecret(key)) => {
            assert!(provisioned.secrets.iter().filter(|s| s.key == key).count() > 1);
        }
        Err(ProvisionRefusal::BadSecretValue(key)) => {
            assert!(provisioned.secrets.iter().any(|s| s.key == key
                && (s.value.is_empty()
                    || s.value.len() > MAX_PROVISION_SECRET_BYTES as usize
                    || control(&s.value)
                    || s.value.trim() != s.value)));
        }
        Err(ProvisionRefusal::TooManySecrets) => {
            assert!(provisioned.secrets.len() > MAX_PROVISION_SECRETS as usize);
        }
        Err(ProvisionRefusal::TooManySummaryLines) => {
            assert!(provisioned.summary.len() > MAX_PROVISION_SUMMARY_LINES as usize);
        }
        Err(ProvisionRefusal::BadSummaryLine(index)) => {
            let line = &provisioned.summary[index];
            assert!(line.len() > MAX_PROVISION_SUMMARY_LINE_BYTES as usize || control(line));
        }
        Err(ProvisionRefusal::SettingsTooLong) => {
            assert!(provisioned.settings_json.len() > MAX_PROVISION_SETTINGS_BYTES as usize);
        }
        Err(
            ProvisionRefusal::SettingsNotObject
            | ProvisionRefusal::BadSettingValue(_)
            | ProvisionRefusal::CredentialLeak,
        ) => {}
    }
}
